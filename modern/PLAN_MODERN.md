# Plan: the Modern machine mode (script, revision 2)

Lead, 2026-11-14. Proposed by the user; **not started**. Replaces revision 1
(2026-11-13, kept at `tmp/refactor/superseded/PLAN_MODERN-20261113.md`).
Speed for DOS games and Windows stays the main goal; the user decides whether
and when each gate opens.

Names: r86 has two machine modes, **Retro** (today's machine, the only one
implemented) and **Modern** (this plan). Their definitions and rules are in
[design/MACHINE_MODES.md](../design/MACHINE_MODES.md); this file is the plan
for building Modern. There is one interpreter: a mode is a CPU model plus
the APIC platform, never a second implementation.

Targets, in order: Windows 7 32-bit, Windows 10 32-bit, Mac OS X 10.6 Snow
Leopard (32-bit kernel, macOS hosts only). Decided by the user on
2026-11-14: Modern is 32-bit and boots through SeaBIOS.

What changed from revision 1: the measurement that decides the project comes
first (Windows 7 on Retro), every later act has an entry condition
and named exits, and the float, NX and APIC contracts are fixed here instead
of being left to the Intern. Scenes that change the emulator carry code
sketches written against the current tree.

## 1. How to read this script

- **Act**: a milestone. **Scene**: one Intern batch with one handoff.
- Every scene lists ENTER (preconditions), ACTION, EVIDENCE (files under
  `tmp/modern/<scene>/`) and EXITS (each exit names the next node).
- **G-nodes** are gates the USER decides. **D-nodes** are technical choices
  Lead decides from the evidence. **L-nodes** are leaves: the script ends
  there and the leaf says what stays in the tree.
- One scene is on stage at a time: there is one Intern, in the main tree
  (AGENTS.md "Intern and Lead"). Nothing here runs in parallel.
- A scene that needs a number not listed in its EXITS stops and asks Lead.
  No exit is improvised.
- **Section 8 is the task list**: one `[ ]` or `[X]` box per task, in scene
  order. It is the place to see what is done and what is next.
- **Snippets** are sketches against commit b00a268. They show placement,
  the contract and the existing idiom to follow. The Intern re-reads the
  cited lines first (line numbers move), may rename, and matches the
  surrounding code. A snippet never overrides section 4; where a snippet
  and the cited code disagree, the code is right and Lead is told.

## 2. Cast, stage and props

| Role | Does | Never does |
|---|---|---|
| USER | Opens gates G0-G5, supplies media, runs the Mac measurement | — |
| Lead | Briefs, reviews, decides D-nodes, commits, UI, `vm_store`, Mac app | Touches the RPi 5 |
| Intern | Every scene's ACTION, RPi 5 runs, handoffs | Git state changes, UI, user disks |

Stage (hosts):

| Host | Engine | Role in this script |
|---|---|---|
| x86 dev VM | interpreter (no x86 JIT exists) | Iteration, callgrind, QEMU tools |
| RPi 5 | interpreter and A64 JIT | ARM correctness; stand-in timing only |
| Mac (Apple silicon) | A64 JIT | The only host whose numbers can pass a gate |

Props (none in Git; all under `tmp/modern/`, recorded with SHA256):

| Prop | Source | State 2026-11-14 |
|---|---|---|
| Windows 7 32-bit SP1 ISO | USER | not on this host |
| Windows 10 32-bit ISO (edition and build recorded) | USER | not on this host |
| Mac OS X 10.6 Server medium, and a loader SeaBIOS can boot | USER | not on this host |
| `win7-seed.vhd`, `win10-seed.vhd` | scenes 1.1, 6.1 | to be made |
| QEMU 10.0.13 `/usr/bin/qemu-system-i386` | installed | TCG only, no `/dev/kvm` |
| QEMU `insn` plugin | built from `refs/qemu/tests/tcg/plugins/insn.c` | built: `tmp/modern/tools/libinsn.so` (a tool; nothing copied into r86) |
| valgrind, iasl | installed | — |

Guard guests (must not get slower or change behaviour): Floppy Bird, Doom,
Windows 2000 frozen seed `tmp/usb/win2000-clean.vhd`, TinyCore, ReactOS.

## 3. Facts and assumptions

Verified in the tree on 2026-11-14:

| # | Fact | Where |
|---|---|---|
| F1 | CPUID is family 6 model 5 (0x652), max leaf 2; EDX has PAE, SEP, CMOV, MMX, FXSR; no APIC bit, no SSE, no extended leaves | `src/x86/interp.rs:409-451`, `cpuid_leaf` 4131 |
| F2 | PAE paging exists; bit 63 is reserved in every PAE entry | `walk_pae` `src/x86/mmu.rs:1716`, masks 242-245 |
| F3 | The JIT's resident translation refuses PAE: native refills and fetch qualification use the slow walker under CR4.PAE | `resident_page` `src/x86/mmu.rs:521` |
| F4 | Every execute page fault sets PFEC bit 4 (debt CPU-2) | `page_fault` `src/x86/mmu.rs:1949` |
| F5 | FPU state: x87 as host `f64`, MMX in a separate array, an `mxcsr` field, no XMM registers; FXSAVE writes the x87 image only | `src/x86/cpu.rs:100-108`, `fxsave` `interp.rs:12520` |
| F6 | x87 has no inexact flag; rounding control applies only to integer conversion | `fpu_round` `interp.rs:13446` |
| F7 | MMX arms raise #UD on 66/F2/F3; `Prefix` is None/Rep/Repne; two-byte opcodes go through `execute_0f` | `reject_sse_prefixes` `interp.rs:8768`, 324, 2957 |
| F8 | The shared JIT decoder compiles unprefixed MMX only and has a test that SSE prefixes are rejected | `src/x86/jit/decode.rs:128`, 1229 |
| F9 | Unknown MSRs raise #GP; IA32_APIC_BASE is plain storage, reset to 0 (debt CPU-3) | `rdmsr`/`wrmsr` `interp.rs:4282-4392` |
| F10 | FEC00000h and FEE00000h are declared MMIO regions with no device | `src/machine.rs:2092-2095` |
| F11 | External interrupts come from one place: `bus.pic.acknowledge()` | `deliver_irq` `src/machine.rs:123` |
| F12 | ACPI: FADT/FACS/RSDT/RSDP built in Rust, loaded by SeaBIOS; DSDT has a static PIC `_PRT`, no `_PIC`, no MADT, no HPET | `src/acpi.rs:154`, `firmware/acpi/dsdt.asl:35` |
| F13 | Disks: LBA28, at most 128 GiB. ATAPI reports the DVD profile for large images; no READ DVD STRUCTURE (ADh) | `src/disk.rs:11`, `src/pc_devices.rs:2366` |
| F14 | USB is UHCI with one HID tablet; no USB keyboard | KNOWN_ISSUES "Limits by design" |
| F15 | VM presets stop at Windows XP and Linux | `src/frontend/guest_os.rs:26-34` |
| F16 | No Apple performance number exists for any workload | `tmp/perf/scoreboard.txt` |
| F17 | tiny386's SSE ignores MXCSR ("TODO: respect mxcsr") | `refs/tiny386/simd.inc:763` and following |
| F18 | RDMSR/WRMSR mask the index with 3FFFh, so EFER (C0000080h) would alias MSR 80h | `canonical_p6_msr_index` `interp.rs:4278`, mask 502 |
| F19 | `execute_0f` is already `#[inline(never)]`: nothing reached from it is inside `run_batch_inner` | `interp.rs:2956` |
| F20 | LDMXCSR, STMXCSR, CLFLUSH and the three fences already execute, ungated; MXCSR mask is FFBFh; offset 160 is named for XMM | `fxsave_group` `interp.rs:9645`, 12248-12249 |
| F21 | JIT translation rechecks hold 32-bit PDE/PTE words and are recorded only by the non-PAE walker | `src/x86/mmu.rs:161`, 1663; `cpu.rs:1950` |
| F22 | With CPUID's APIC bit set, SeaBIOS enables the local APIC, sets LINT0 to ExtINT (8700h) and LINT1 to NMI (8400h), broadcasts INIT/SIPI, then waits for the CMOS CPU count, which is 1 | `vendor/seabios/src/fw/smp.c:109-152` |
| F23 | QEMU's MADT marks the PCI interrupt lines (IRQ 5, 9, 10, 11) level, active high, and keeps the PIC-mode `_PRT` numbers | `refs/qemu/hw/i386/acpi-common.c:132-139` |
| F24 | Device time is `TICKS_PER_SECOND = 16 * 1_193_182`; devices report their next event tick to `next_device_event` | `src/devices/mod.rs:10`, `src/machine.rs:2413` |
| F25 | ISA interrupt lines change in one place, `drive_irq`; unclaimed MMIO ends in `unclaimed_memory` | `src/machine.rs:2314`, 2071 |
| F26 | The QEMU `insn` plugin builds from `refs/qemu` and loads in the installed QEMU 10.0.13 (checked 2026-11-14) | `tmp/modern/tools/libinsn.so` |
| F27 | Firmware is SeaBIOS 1.17.0 (`rel-1.17.0-r86-3`), built with `CONFIG_QEMU`, PnP BIOS off, everything else at SeaBIOS defaults: SMBIOS on, ATA DMA off (INT 13h reads use PIO), AHCI unused. The VGA ROM is SeaVGABIOS for Bochs VBE with the emulator fix-ups and its own stack enabled by default | `firmware/seabios/README.md`, `bios.config`, `vga.config`; `vendor/seabios/src/Kconfig:152`, `vgasrc/Kconfig:143-157` |
| F28 | CD boot (El Torito) and MBR disk boot through this ROM already work for Windows 2000, ReactOS, TinyCore and FreeDOS | `firmware/seabios/README.md` "Validation", `benchmarks/README.md` |

Assumptions (from memory or documents, **not verified**). Each names the
scene that tests it; no code is written on the strength of an assumption.

| # | Assumption | Tested in |
|---|---|---|
| A1 | Windows 7 32-bit runs without SSE, SSE2 or NX (Pentium II feature set) | 0.3 |
| A2 | With no MADT, Windows 7 loads its PIC ACPI HAL and the non-PAE kernel | 1.2 |
| A3 | Windows 10 32-bit refuses to boot without PAE, NX and SSE2, and needs an APIC | 0.3, 6.2 |
| A4 | Advertising NX makes Windows XP SP2+ and Windows 7 choose the PAE kernel | 3.8 |
| A5 | Windows XP's APIC HAL writes TPR on every IRQL change; Windows 7/10 do so far less | 4.3 |
| A6 | Windows 2000/XP bind the HAL at install; Windows 7/10 choose it at each boot | 4.3 |
| A7 | A Windows 7/10 disk installed under QEMU (i440FX, PIIX3 IDE) boots on r86 | 1.2, 6.2 |
| A8 | A QEMU `vpc` dynamic disk is readable as an r86 dynamic VHD | 0.2 |
| A9 | TinyCore's 32-bit kernel does not enable PAE, so it cannot exercise NX | 3.2 |
| A10 | Snow Leopard needs SSE3, an APIC, an SMC with the OSK key and a USB keyboard, and a loader started by SeaBIOS can boot the 10.6 Server medium's 32-bit kernel without EFI firmware; HPET and PIIX IDE support unknown | 8.1 |
| A11 | Windows 7 and 10 are usable on a Mac with the A64 JIT | 1.4, 6.4 |
| A12 | Windows 7's and 10's built-in display drivers change modes by running the VGA BIOS's INT 10h VBE calls at run time, and this SeaVGABIOS works there (the same ROM configuration is QEMU's default VGA) | 1.2, 6.2 |
| A13 | Windows 10 32-bit installs and boots from BIOS on an MBR disk | 0.3, 6.1 |

## 4. Fixed contracts (decided here, not per scene)

1. **Two knobs during development, one machine mode at the end.**
   `--cpu-model pentium2|modern` and `--apic` exist only until act 7, where
   they collapse into the machine mode (`retro` or `modern`), fixed at VM
   creation (A6: a Windows
   2000/XP install cannot change HAL afterwards). CPUID's APIC bit and
   IA32_APIC_BASE follow the device, on either CPU model.
2. **Defaults do not move.** Every existing VM, seed and benchmark keeps the
   Pentium II model and the PIC-only machine. The one intended exception is
   F4: PFEC bit 4 becomes "PAE and EFER.NXE only" on both models, which
   removes debt CPU-2.
3. **16-bit stays in the interpreter** (AGENTS.md): BIOS boot, the boot
   manager and NTVDM. No generator compiles real mode, V86 or 16-bit code
   segments.
4. **SSE lives outside `run_batch_inner`**: its own module, reached by one
   cold call from `execute_0f`. Plan-intern item 4c's rule ("nothing else is
   added to the loop") holds whether or not 4c has landed.
5. **Float bar** (matches the project's x87, F6, and exceeds F17):
   - Results are bit-exact with x86 under round-to-nearest: x86 NaN
     propagation, the negative QNaN "real indefinite", MINPS/MAXPS operand
     order, integer indefinite 80000000h, DAZ and FTZ.
   - Other MXCSR rounding modes use a slow software path, still bit-exact.
   - IE, ZE, DE, OE and UE status flags are set from operand and result
     classification.
   - The inexact flag (PE) is not tracked, and unmasked SIMD exceptions (#XM)
     are not delivered. Both are listed under KNOWN_ISSUES "Limits by design".
   - No host floating-point environment is read or written. Rust assumes the
     default environment, and wasm and no_std have none.
   - ARM, RISC-V and wasm hosts canonicalise NaNs and saturate conversions:
     every result that can differ is produced by explicit code, never by the
     host operation alone.
6. **A gate passes only on a Mac measurement.** RPi 5 and x86 numbers may
   reject (D-nodes) and may stand in while iterating; they never pass G1, G3
   or G5 (AGENTS.md: Linux/RPi results do not establish an Apple benefit).
7. **Blockers are fixed as emulator bugs, in the shared path.** No
   per-guest special case, no retry through another engine.
8. **Media and seeds stay local.** A seed is written once, its SHA256
   recorded, then booted only with `--hdd-frozen`.
9. **BIOS only.** SeaBIOS is the only firmware, and the same ROM files
   serve Retro and Modern (F27).
   - Every guest boots the BIOS way: El Torito from CD, MBR from disk. r86
     provides no UEFI or EFI firmware, no EFI services and no GPT-only boot.
   - Seeds and QEMU rehearsals use SeaBIOS too (QEMU's default firmware for
     `-M pc`), never OVMF. A disk installed under UEFI is not a seed.
   - What a guest needs from firmware is therefore the BIOS surface: INT 13h
     with extensions, INT 15h E820, INT 10h VBE, the ACPI tables SeaBIOS
     relocates, SMBIOS. A gap there is a blocker like any other (table B).
   - The display of a Modern guest is VBE through the VGA BIOS, at boot and
     at run time (A12). No guest display driver is part of this plan.
   - A ROM change (configuration or patch) is a change to Retro: it needs
     Lead's agreement, the rebuild procedure of `firmware/seabios/README.md`
     and Retro's firmware checks in `design/VALIDATION.md`. None is expected
     (F22).

## 5. Measurement protocol M and thresholds T

Used by scenes 1.4, 3.8, 4.3 and 6.4. Rules of `benchmarks/README.md` apply:
release build without `jit-profile`, fresh process per sample, one pilot to
reject, two measured runs to confirm.

| Id | Measurement | How |
|---|---|---|
| M1 | Cold boot | `--clock realtime`, frozen seed, reset to the recorded desktop frame (auto-logon, plain wallpaper, taskbar clock hidden). Seconds, CPU seconds and guest steps |
| M2 | Interaction | `script/ws_cli.py` input, then frame match: Start menu open; Notepad launched; Explorer "Computer" window open. Seconds each |
| M3 | Idle | 5 minutes after M1: share of time halted, steps per second |
| M4 | Profile (separate diagnostic run) | `jit-profile` build with `--perf-counters`: native and interpreted step share, interpreted entries by opcode, 16-bit and V86 share, exits per 1M steps, slow page walks |
| M5 | Guest instruction census | QEMU TCG with the `insn` plugin: instructions from reset to the same desktop frame, and per idle minute |

Hosts for M1-M3: x86 interpreter; RPi 5 interpreter and JIT (Intern, under
`flock /dev/shm/r86-bench.lock`); Mac JIT (USER runs one command Lead
prepares; the seed is copied by hand, never through CI).

Thresholds T, **Lead's proposal; the USER sets the final values at G0**:

| | Windows 7 | Windows 10 |
|---|---|---|
| M1 on the Mac | 120 s | 240 s |
| M2 Start menu | 1.0 s | 2.0 s |
| M2 Notepad | 3 s | 6 s |
| M3 halted share | 80% | 60% |

PASS: every row met. MARGINAL: every row within 2x (idle within 20 points).
FAIL: anything worse.

Performance gate P for the default machine (scenes 3.2-3.6 and 4.1):

- callgrind host instructions per guest instruction, before (a `git archive`
  export under `tmp/build/`, deleted after the build) and after: Floppy Bird
  0..70M, Doom 250M..300M, and a fixed step window of the Windows 2000 frozen
  seed boot. Each within 1%.
- One RPi 5 pair, interpreter and JIT, on the Windows 2000 window-switch
  workload. The Start-menu fade is not a gate (placement moves it 7%).
- New CPU state is appended after existing `Cpu` fields; no hot field offset
  moves.

Gate P commands. The harness to reuse is `tmp/refactor/sti/callgrind.py`
(it parses the profile and prints host instructions per guest step):

```sh
# Baseline: the last commit, exported, built, then the export is deleted.
mkdir -p tmp/build/base tmp/modern/P
git archive HEAD | tar -x -C tmp/build/base
(cd tmp/build/base && CARGO_TARGET_DIR=/work/r86/tmp/target-intern/base \
   cargo build --locked --release --no-default-features --features host --bin r86-run)
cp tmp/target-intern/base/release/r86-run tmp/modern/P/base-host
rm -rf tmp/build/base
# Candidate: the working tree.
CARGO_TARGET_DIR=tmp/target-intern \
  cargo build --locked --release --no-default-features --features host --bin r86-run
cp tmp/target-intern/release/r86-run tmp/modern/P/candidate-host

# One run per binary per workload; Floppy Bird shown.
taskset -c 2 valgrind --tool=callgrind --dump-instr=yes \
  --callgrind-out-file=tmp/modern/P/floppy-base.out tmp/modern/P/base-host \
  --bios firmware/seabios/bios.bin --vga-bios firmware/seabios/vgabios-stdvga.bin \
  --cpu-engine interp --clock deterministic --mem-mib 128 \
  --hdd <floppy.img> --hdd-frozen --boot-order disk --no-cdrom --steps 70000000
# Windows 2000 window: same, with
#   --hdd tmp/usb/win2000-clean.vhd --hdd-frozen
#   --vga-bios firmware/seabios/vgabios-handheld.bin --steps <fixed N>
```

## 6. The decision tree

```
G0 USER: start?
├─ no ─────────────────────────────────────────────► L0
└─ yes: thresholds T fixed, Windows 7 ISO supplied
    │
    ACT 0  PROLOGUE (no emulator change)
    0.1 props ► 0.2 preflight ► 0.3 rehearsal in QEMU ► 0.4 census
    │      0.3: Windows 7 fails on -cpu pentium2 (A1 false)
    │           └─► D0: which feature? ► that act 3 scene before 1.2
    │
    ACT 1  WINDOWS 7 ON RETRO
    1.1 seed in QEMU ► 1.2 first boot in r86
    │      ├─ desktop ───────────────────────────────► 1.4
    │      └─ symptom ► 1.3 blocker loop (table B)
    │             ├─ fixed ──────────────────────────► 1.2
    │             └─ budget spent or out of scope ───► G1 as "blocked"
    1.4 measure M1-M4 ► 1.5 PAE probe
    │
    G1 USER: Windows 7 verdict (Mac numbers against T)
    ├─ FAIL or blocked ──────────────────────────────► L1
    ├─ MARGINAL ► 2.1 cost list ► USER: interlude?
    │      ├─ yes ► act 5 scenes named in 2.1 ► 1.4 ► G1 (once)
    │      └─ no ────────────────────────────────────► L1
    └─ PASS ► D1: Windows 10 forecast (0.4 census x 1.4 rate)
           ├─ forecast FAIL ─────────────────────────► L2
           └─ forecast PASS or MARGINAL
               │
               D2: act order
               ├─ plan-intern 4c and 4e landed, or the USER pauses item 4
               │      ► act 3 then act 4
               └─ otherwise ► act 4 then act 3
               │
    ACT 3  MODERN CPU MODEL
    3.1 model, CPUID, MSRs ► 3.2 NX ► 3.3 FXSAVE and MXCSR
    ► 3.4 SSE/SSE2 moves and integer ► 3.5 SSE/SSE2 float ► 3.6 SSE3
    ► 3.7 JIT decoder audit ► 3.8 guests and Modern versus Retro
    │      any scene: gate P fails
    │           ├─ fixed in one revision ► continue
    │           └─ not fixed ► scene reverted ► Lead ► G2 early
    │
    G2 USER: Modern penalty on Windows 7 (3.8: Modern / Retro, M1 and M2)
    ├─ ratio <= 1.10 ────────────────────────────────► next act
    ├─ ratio > 1.10, and <= 2.0 or one cost >= 30% of cycles
    │      ► D3: act 5 interlude(s) by measured share
    │      ├─ recovered to <= 1.25 ──────────────────► next act
    │      └─ not recovered ─────────────────────────► L2b
    └─ ratio > 2.0 and no single cost >= 30% ────────► L2b
               │
    ACT 4  APIC PLATFORM
    4.1 local APIC, I/O APIC, delivery ► 4.2 MADT
    ► 4.3 guests (TinyCore, Windows 2000, Windows 7)
    │      ├─ Windows 7 APIC slower than PIC by > 5% ► D4 (I-TPR or
    │      │      keep Windows 7 on PIC); continue
    │      ├─ a guest needs HPET (evidence) ► 4.4 HPET ► 4.3
    │      └─ Windows 2000 seed changes behaviour ► fix before leaving
    │
    ACT 6  WINDOWS 10
    6.1 seed in QEMU ► 6.2 first boot in r86
    │      ├─ desktop ───────────────────────────────► 6.4
    │      └─ symptom ► 6.3 blocker loop (table B) ► 6.2
    │             └─ budget spent ───────────────────► G3 as "blocked"
    6.4 measure M1-M4
    │
    G3 USER: Windows 10 verdict
    ├─ FAIL or blocked ──────────────────────────────► L2c
    ├─ MARGINAL ► D3 interlude(s) ► 6.4 ► G3 (once)
    └─ PASS
        │
    ACT 7  MACHINE MODE (Lead)
    7.1 installers inside r86 ► 7.2 type, presets, docs ──► L3
        │
    G4 USER: start Snow Leopard? (licence, OSK source, loader prop)
    ├─ no ───────────────────────────────────────────► L3
    └─ yes
    ACT 8  SNOW LEOPARD, macOS HOSTS ONLY
    8.1 rehearsal in QEMU ► D6: device list
    │      └─ list needs something out of scope (AHCI, x86-64, EFI
    │         firmware) ─────────────────────────► L3
    8.2 devices ► 8.3 first boot and blocker loop ► 8.4 measure
    G5 USER: verdict ── PASS ► L4 │ otherwise ► L3 (devices removed)
```

Leaves:

| Leaf | Outcome | What stays in the tree |
|---|---|---|
| L0 | Not started | Nothing |
| L1 | Windows 7 not viable | Bug fixes from 1.3. `TODO.md` "SSE then SSE2" stays an independent item for XP applications |
| L2 | Windows 7 on Retro | L1's fixes plus a "Windows 7" Retro preset. No CPU model, no APIC, no Modern mode |
| L2b | Modern CPU too slow for Windows 7/10 | L2, plus `--cpu-model modern` without a preset: it serves the independent `TODO.md` SSE item for Windows XP applications. The APIC, if already landed, goes through D5 |
| L2c | Windows 10 not viable | L2b; the APIC stays only if D5 finds a guest that benefits |
| L3 | Modern mode ships | Windows 7 and Windows 10 presets; the mode setting replaces the two development options |
| L4 | L3 plus Snow Leopard | Mac-only devices behind `cfg(target_os = "macos")` |

## 7. Table B: blocker loop (scenes 1.3, 6.3, 8.3)

One blocker per batch: reproduce on the frozen seed, read `refs/tiny386`
first, then `refs/v86`, then Bochs/86Box/QEMU (study only), fix in the
shared path, add the smallest focused test, re-enter the boot scene.

| Symptom | First suspect | Start reading |
|---|---|---|
| Boot manager error before the kernel loads | INT 13h extensions, E820 map, A20, RAM above 256 MiB | SeaBIOS source, `src/devices/firmware.rs` |
| "A required CD/DVD drive device driver is missing" | ATAPI command set (F13: ADh missing) | `src/pc_devices.rs:1995` and following |
| STOP 5Dh, or boot error C0000260h | A CPUID feature the kernel requires | parameters name it; scene 3.1 |
| STOP A5h ACPI_BIOS_ERROR | A table or AML object Windows requires | parameters name it; `src/acpi.rs`, `dsdt.asl` |
| STOP 7Bh INACCESSIBLE_BOOT_DEVICE | IDE identify, DMA, PCI IDE class | `src/pc_devices.rs` |
| STOP 5Ch HAL_INITIALIZATION_FAILED | APIC, timer calibration, PM timer | scene 4.1; `src/acpi.rs` |
| STOP 7Eh, 8Eh, 50h, 1Eh | A CPU semantic bug | lockstep against QEMU at the faulting EIP |
| #GP on RDMSR/WRMSR | An MSR the model must have (F9) | debug ring shows the index |
| Hang with interrupts flowing | Timer source: PIT, RTC, PM timer, LAPIC timer | `src/machine/clock.rs` |
| Hang with the CPU spinning | Calibration loop or missing device status bit | port and MMIO diagnostics |
| Desktop stays at 640x480 in 16 colours, or a mode change fails | The VGA BIOS's INT 10h VBE code run by the guest in V86 mode or its own x86 emulator (A12) | `vendor/seabios/vgasrc`, `src/devices/vga.rs`; compare the same call under QEMU |
| Boot manager draws text progress instead of its animation | VBE mode list at boot | Not a blocker: record it |
| Guest reports no or wrong system information; setup refuses the machine | SMBIOS tables (F27) | `vendor/seabios/src/fw/smbios.c` |
| Works, but minutes per step of setup | Not a blocker: record under M4 | — |

Budget per loop: 5 blockers or 5 Intern batches, whichever comes first.
Then the scene exits to its gate as "blocked", with the open symptom and the
evidence. Out of scope at any budget: x86-64, SMP, AHCI, UEFI (contract 9).

## 8. Task list

One box per task. The scenes below say how; this list says what is left.

- `[X]` done, with the date and where the evidence is. `[ ]` open.
- Lead ticks a box when the evidence exists (a handoff, a commit, a recorded
  decision). The Intern reports; it does not tick.
- A task that turns out not to be needed is ticked as `[X] skipped:` with
  the reason. Conditional tasks say what enters them.
- Ids are `scene.n`. A USER task is a decision or a prop only the user can
  give.

### Preparation

- [X] P.1 Revision 2 of this plan written; revision 1 archived
  (2026-11-14, `tmp/refactor/superseded/PLAN_MODERN-20261113.md`).
- [X] P.2 Modes named Retro and Modern and documented (2026-11-14,
  `AGENTS.md`, `design/MACHINE_MODES.md`).
- [X] P.3 USER decision: Modern is 32-bit and boots through SeaBIOS
  (2026-11-14, contract 9).
- [X] P.4 Facts F1-F28 checked against the tree at b00a268 (2026-11-14).
- [X] P.5 Host tools present: QEMU 10.0.13 (TCG), valgrind, iasl
  (2026-11-14).
- [X] P.6 QEMU `insn` plugin built and loaded once (2026-11-14, F26).
- [X] P.7 Plan and mode documents committed (2026-11-14, with this
  revision).

### G0: start (USER)

- [ ] G0.1 USER: say start.
- [ ] G0.2 USER: set the thresholds T of section 5 (or accept Lead's).
- [ ] G0.3 USER: supply the Windows 7 32-bit SP1 ISO.
- [ ] G0.4 USER: name the Mac that runs the gate measurements.
- [ ] G0.5 Lead: add one line to `tmp/refactor/plan-intern.txt` and brief
  scene 0.1.

### Act 0: prologue

- [ ] 0.1.1 Copy the ISO to `tmp/modern/media/`; record SHA256, edition
  and language.
- [X] 0.1.2 Build the `insn` plugin (2026-11-14, P.6).
- [ ] 0.1.3 Build `unattend.iso` with the `autounattend.xml` settings of
  scene 0.4.
- [ ] 0.2.1 Build `r86-run` into `tmp/target-intern`.
- [ ] 0.2.2 (a) TinyCore at 2048 MiB reaches its marker and reports
  MemTotal.
- [ ] 0.2.3 (b) A QEMU-made dynamic VHD opens frozen in r86 (A8).
- [ ] 0.2.4 (c) The Windows 7 ISO reaches its boot manager in r86; frame
  saved.
- [ ] 0.3.1 Run 1: Windows 7 on QEMU `-cpu pentium2` (A1); result line
  written.
- [ ] 0.3.2 Runs 2-4: Windows 10 on `pentium2`, `coreduo`, `coreduo`
  without SSE3 (A3, A13). Waits for 6.1.1.
- [ ] 0.3.3 Lead: D0, only if run 1 fails.
- [ ] 0.4.1 Create `win7-seed.vhd` and install unattended under QEMU.
- [ ] 0.4.2 Guest settings: auto-logon, plain wallpaper, taskbar clock
  hidden; shut down; record the seed's SHA256.
- [ ] 0.4.3 Census run 1 (reset to desktop) and run 2 (five idle minutes).
- [ ] 0.4.4 `census.txt`: instructions to desktop, idle rate, commands.

### Act 1: Windows 7 on Retro

- [ ] 1.2.1 First boot of the frozen seed in r86 (x86, interpreter).
- [ ] 1.2.2 Record the HAL and the IDE controller Windows chose (A2, A7).
- [ ] 1.2.3 Change the display to 1024x768, 32-bit colour (A12).
- [ ] 1.2.4 Record the DESKTOP and MENU frame hashes at that mode.
- [ ] 1.3.1 Blocker loop closed; each blocker listed in the handoff with
  its fix, test and KNOWN_ISSUES entry. Enters on any table B symptom.
- [ ] 1.4.1 Build the aarch64 timing and profile binaries.
- [ ] 1.4.2 M1-M3 on the x86 interpreter.
- [ ] 1.4.3 M1-M3 on the RPi 5 interpreter.
- [ ] 1.4.4 M1-M3 on the RPi 5 JIT.
- [ ] 1.4.5 M4 profile on the RPi 5 JIT.
- [ ] 1.4.6 Lead: write the one command for the Mac run.
- [ ] 1.4.7 USER: run M1-M3 on the Mac; send the output.
- [ ] 1.4.8 Calibration: r86 steps to desktop over the QEMU census count.
- [ ] 1.4.9 Lead: first Windows 7 and first Apple rows in
  `tmp/perf/scoreboard.txt`.
- [ ] 1.5.1 Force PAE in the guest, reboot inside one process, confirm it.
- [ ] 1.5.2 M1 and M2 under PAE on the RPi 5, JIT and interpreter.
- [ ] 1.5.3 PAE/non-PAE ratio and slow page walks per 1M steps.

### G1: Windows 7 verdict

- [ ] G1.1 Lead: one-page summary (table against T, PAE ratio, open
  blockers).
- [ ] G1.2 USER: PASS, MARGINAL or FAIL.
- [ ] G1.3 Scene 2.1 cost list. Enters on MARGINAL.
- [ ] G1.4 Lead: D1, the Windows 10 forecast (needs 6.1.3).
- [ ] G1.5 Lead: D2, the act order.

### Act 3: Modern CPU model

- [ ] 3.1.1 `CpuModel`, `apic` and `msr_efer` appended to `Cpu`;
  `Machine::set_cpu_model`.
- [ ] 3.1.2 `--cpu-model`, config key `cpu-model`, usage text,
  `design/CONFIGURATION.md`.
- [ ] 3.1.3 Modern CPUID table; leaf 2, 4 and 8000000xh values recorded
  with their source.
- [ ] 3.1.4 MSR index no longer masked on the modern model (F18); test
  that the Pentium II aliases exactly as before.
- [ ] 3.1.5 EFER with NXE only, invalidating translations and JIT links on
  change.
- [ ] 3.1.6 CR4 mask per model (OSXMMEXCPT).
- [ ] 3.1.7 Unknown MSR indexes visible in the debug ring.
- [ ] 3.1.8 CPUID and MSR unit tests for both models.
- [ ] 3.1.9 Gate P; no_std machine and wasm cfg builds.
- [ ] 3.2.1 Bit 63 reserved or NX by EFER.NXE, 4 KiB and 2 MiB paths.
- [ ] 3.2.2 NX refused on fetch in `check_perm_pae`.
- [ ] 3.2.3 PFEC bit 4 only with PAE and NXE, both models (debt CPU-2).
- [ ] 3.2.4 Tests: fetch fault and code, NXE-clear reserved fault, 2 MiB
  page, non-PAE negative case, JIT block on a page that becomes NX.
- [ ] 3.2.5 TinyCore: does its kernel report NX (A9)?
- [ ] 3.2.6 Gate P, including the Windows 2000 window.
- [ ] 3.3.1 XMM0-7 appended to `FpuState`.
- [ ] 3.3.2 FXSAVE/FXRSTOR write and read 288 bytes on the modern model.
- [ ] 3.3.3 MXCSR mask FFFFh and reserved-bit #GP on the modern model.
- [ ] 3.3.4 LDMXCSR/STMXCSR gated on the modern model; Pentium II
  unchanged.
- [ ] 3.3.5 Tests; gate P.
- [ ] 3.4.1 `src/x86/sse.rs` with its entry arms in `execute_0f`.
- [ ] 3.4.2 Gating: #UD, #NM, #GP on a misaligned aligned form.
- [ ] 3.4.3 Moves, non-temporal stores, shuffles and unpacks.
- [ ] 3.4.4 XMM integer operations on the MMX lane functions, plus the
  operations with no MMX counterpart.
- [ ] 3.4.5 The MMX-register additions of SSE and SSE2; MOVNTI.
- [ ] 3.4.6 Differential harness on the x86_64 host; golden file written.
- [ ] 3.4.7 Golden file replayed on the RPi 5.
- [ ] 3.4.8 Gate P.
- [ ] 3.5.1 Arithmetic and square root with x86 NaN rules.
- [ ] 3.5.2 MIN/MAX, compares, COMISS/UCOMISS and double forms.
- [ ] 3.5.3 Conversions with MXCSR rounding and the integer indefinite.
- [ ] 3.5.4 Directed rounding: the one integer routine, both widths.
- [ ] 3.5.5 DAZ, FTZ and the IE, ZE, DE, OE, UE flags.
- [ ] 3.5.6 Harness extended (NaN, SNaN, denormal, infinity, out of range,
  four rounding modes); replayed on the RPi 5.
- [ ] 3.5.7 KNOWN_ISSUES "Limits by design": PE not tracked, #XM not
  delivered.
- [ ] 3.5.8 Gate P.
- [ ] 3.6.1 Lead: build or skip SSE3, from 0.3.2 and G4.
- [ ] 3.6.2 SSE3 instructions and FISTTP; harness extended.
- [ ] 3.7.1 Decoder test for the whole two-byte SIMD map.
- [ ] 3.7.2 One JIT boot on the RPi 5 with `--cpu-model modern`.
- [ ] 3.8.1 TinyCore to a shell under both models.
- [ ] 3.8.2 Windows 2000 frozen seed under `pentium2`: frames and steps
  unchanged.
- [ ] 3.8.3 Windows 7 seed under `modern`: kernel chosen (A4), M1, M2, M4.
- [ ] 3.8.4 Modern/Retro ratio and the SSE opcode histogram.

### G2: Modern penalty

- [ ] G2.1 Lead: summary of 3.8.4.
- [ ] G2.2 USER: continue, interlude or stop.
- [ ] G2.3 Lead: D3, which interludes. Enters when the ratio is above
  1.10.

### Act 5: JIT interludes (each enters only when D3 or scene 2.1 names it)

- [ ] 5.1 I-PAE: PAE arm of `resident_page`, 64-bit rechecks; pilot, then
  two confirming runs.
- [ ] 5.2 I-SSE: helper-call lowering for the top of the histogram; pilot,
  then two confirming runs.
- [ ] 5.3 I-TPR: TPR write without leaving the block; pilot, then two
  confirming runs.
- [ ] 5.4 I-shared: the deferred "Windows JIT fallbacks" item.

### Act 4: APIC platform

- [ ] 4.1.1 `src/devices/apic.rs`: local APIC registers, priority, timer.
- [ ] 4.1.2 I/O APIC: 24 entries, edge and level, remote IRR, EOI.
- [ ] 4.1.3 `attach_apic`, `--apic`, config key `apic`, usage text.
- [ ] 4.1.4 IA32_APIC_BASE reset value and write rule; CPUID bit follows
  the device.
- [ ] 4.1.5 Machine hunks: interrupt source, `drive_irq` and the PIT edge,
  timer events, MMIO.
- [ ] 4.1.6 SeaBIOS boots with `--apic` and leaves virtual-wire mode set
  (F22).
- [ ] 4.1.7 Device unit tests; machine test that an untouched APIC changes
  nothing.
- [ ] 4.1.8 Gate P.
- [ ] 4.2.1 `madt()` and the RSDT and loader changes behind `--apic`.
- [ ] 4.2.2 TinyCore with `--apic`: local APIC timer and IO-APIC routing in
  dmesg; NIC, IDE and USB interrupts work.
- [ ] 4.2.3 Without `--apic`: serial output identical to before.
- [ ] 4.3.1 Windows 2000 frozen seed with `--apic` boots as before.
- [ ] 4.3.2 Windows 7 seed with `--apic`: HAL noted (A6); M1, M2, M4 with
  TPR writes per 1M steps (A5).
- [ ] 4.3.3 Lead: D4. Enters when APIC is more than 5% slower than PIC.
- [ ] 4.4.1 HPET. Enters only with evidence from 4.3, 6.3 or 8.1.

### Act 6: Windows 10

- [ ] 6.1.1 USER: supply the Windows 10 32-bit ISO; edition and build
  recorded.
- [ ] 6.1.2 `win10-seed.vhd` installed under QEMU, BIOS and MBR; SHA256
  recorded.
- [ ] 6.1.3 Census for Windows 10.
- [ ] 6.2.1 First boot in r86 with `--cpu-model modern --apic`.
- [ ] 6.2.2 Display mode change works (A12).
- [ ] 6.3.1 Blocker loop closed. Enters on any table B symptom.
- [ ] 6.4.1 M1-M4 on x86 and the RPi 5.
- [ ] 6.4.2 USER: M1-M3 on the Mac.

### G3: Windows 10 verdict

- [ ] G3.1 Lead: summary.
- [ ] G3.2 USER: PASS, MARGINAL or FAIL.
- [ ] G3.3 Lead: D5, whether the APIC stays. Enters on the way to L2b or
  L2c.

### Act 7: machine mode

- [ ] 7.1.1 Windows 7 installed from its ISO inside r86, x86 and Mac; wall
  time and the INT 13h share recorded.
- [ ] 7.1.2 Windows 10 the same.
- [ ] 7.2.1 Lead: mode field in `vm_store` (`retro`, `modern`; missing
  means Retro), fixed at creation.
- [ ] 7.2.2 Lead: `--cpu-model` and `--apic` replaced by the mode setting.
- [ ] 7.2.3 Lead: "Windows 7" and "Windows 10" presets; availability per
  frontend from the recorded numbers.
- [ ] 7.2.4 Lead: documents (KNOWN_ISSUES, `design/CONFIGURATION.md`,
  `design/MACHINE_MODES.md`, `design/IMPLEMENTATION_DEBT.md`, `TODO.md`,
  scoreboard).
- [ ] 7.2.5 Lead: retire this plan to local backups.

### G4 and Act 8: Snow Leopard, macOS hosts only

- [ ] G4.1 USER: start or not.
- [ ] G4.2 USER: the 10.6 Server medium.
- [ ] G4.3 USER: where the OSK key comes from.
- [ ] G4.4 USER: the BIOS-started loader, as a prop.
- [ ] G4.5 USER: thresholds T for Snow Leopard.
- [ ] 8.1.1 QEMU rehearsal with SeaBIOS and the loader; one run per
  removed device (HPET, AHCI, USB keyboard, SMC).
- [ ] 8.1.2 Census.
- [ ] 8.1.3 Lead: D6, the minimum device list.
- [ ] 8.2.1 USB HID keyboard on UHCI (every host).
- [ ] 8.2.2 SMC and the machine profile, macOS builds only.
- [ ] 8.2.3 HPET through 4.4.1, if D6 lists it.
- [ ] 8.3.1 First boot; blocker loop closed.
- [ ] 8.4.1 M1-M3 on the Mac.
- [ ] G5.1 USER: verdict.

---

## ACT 0. Prologue: props and rehearsal (no emulator change)

### Scene 0.1: props

- ENTER: G0 is yes; the USER has set T and supplied the Windows 7 ISO.
- ACTION: copy media to `tmp/modern/media/`, record SHA256, edition and
  language. Build the QEMU `insn` plugin outside the r86 build.

  ```sh
  mkdir -p tmp/modern/media tmp/modern/tools
  sha256sum tmp/modern/media/*.iso > tmp/modern/media/SHA256SUMS
  # GPL source built as a tool; nothing from it enters r86. Built once
  # already (F26); rebuild only if QEMU is upgraded.
  gcc -O2 -shared -fPIC -I refs/qemu/include/qemu \
    $(pkg-config --cflags glib-2.0) \
    refs/qemu/tests/tcg/plugins/insn.c -o tmp/modern/tools/libinsn.so
  ```
- EXITS: done ► 0.2.

### Scene 0.2: preflight on today's tree

- ACTION: (a) TinyCore at `--mem-mib 2048` reaches its shell and reports the
  RAM; (b) `qemu-img create -f vpc -o subformat=dynamic` makes a 24 GiB disk
  that r86 opens (A8); (c) r86 attaches the Windows 7 ISO and SeaBIOS starts
  its boot loader.

  ```sh
  CARGO_TARGET_DIR=tmp/target-intern cargo build --locked --release --bin r86-run
  R=tmp/target-intern/release/r86-run
  FW="--bios firmware/seabios/bios.bin --vga-bios firmware/seabios/vgabios-stdvga.bin"

  # (a) large RAM: the marker boot of design/VALIDATION.md at 2048 MiB,
  #     then MemTotal read back over the serial console.
  $R $FW --firmware-display serial --mem-mib 2048 --clock deterministic \
    --iso tmp/benchmarks/tinycore_boot/assets/tinycore-marker.iso \
    --cmdline 'console=ttyS0,115200n8 loglevel=3 nozswap nofstab' \
    --serial-command 'grep MemTotal /proc/meminfo' --expect-serial MemTotal

  # (b) a QEMU-made dynamic VHD opens read-only in r86.
  qemu-img create -f vpc -o subformat=dynamic tmp/modern/0.2/a8-probe.vhd 24G
  $R $FW --hdd tmp/modern/0.2/a8-probe.vhd --hdd-frozen --no-cdrom --steps 50000000

  # (c) the installer's boot manager starts.
  $R $FW --iso tmp/modern/media/win7.iso --boot-order cdrom --mem-mib 1024 \
    --ws-listen 127.0.0.1:8790 &
  python3 script/ws_cli.py --url ws://127.0.0.1:8790 frame tmp/modern/0.2/bootmgr.ppm
  ```
- EXITS:
  - all three hold ► 0.3;
  - (a) fails ► fix as a blocker (large RAM is needed by every later act);
  - (b) fails ► seeds use raw images; ► 0.3;
  - (c) fails ► table B row 1, counted against act 1's budget.

### Scene 0.3: requirement rehearsal in QEMU

Purpose: replace A1 and A3 with observations before any CPU work.

- ACTION, each a QEMU TCG boot of the installer to its first screen:
  1. Windows 7, `-cpu pentium2` (no SSE, no NX).
  2. Windows 10, `-cpu pentium2`; expected to refuse.
  3. Windows 10, `-cpu coreduo`; expected to run.
  4. Windows 10, `-cpu coreduo` minus SSE3; tells whether SSE3 is needed.
  Runs 2-4 wait until the USER supplies the Windows 10 ISO; run 1 alone
  unblocks act 1.

  ```sh
  Q="qemu-system-i386 -M pc -accel tcg -display none -vnc 127.0.0.1:5 \
     -monitor unix:tmp/modern/0.3/mon.sock,server,nowait -boot d"
  $Q -m 1024 -cpu pentium2         -cdrom tmp/modern/media/win7.iso    # run 1
  $Q -m 2048 -cpu pentium2         -cdrom tmp/modern/media/win10.iso   # run 2
  $Q -m 2048 -cpu coreduo          -cdrom tmp/modern/media/win10.iso   # run 3
  $Q -m 2048 -cpu coreduo,pni=off  -cdrom tmp/modern/media/win10.iso   # run 4
  # Evidence: `screendump tmp/modern/0.3/runN.ppm` on the monitor socket.
  ```
  QEMU always supplies an APIC and a MADT, so these runs test CPU features
  only. A2 (the PIC HAL) is tested in r86, scene 1.2.
- EVIDENCE: one line per run: command, result, stop code and parameters.
- EXITS:
  - run 1 boots ► 0.4;
  - run 1 fails ► D0: the stop code names the feature. Scene 0.4 runs with
    the smallest QEMU CPU that boots; that part of act 3 (for example scene
    3.4 for SSE) is done before scene 1.2. The act order in D2 is then
    already decided: act 3 first.

### Scene 0.4: guest instruction census (M5)

- ACTION: install Windows 7 in QEMU to the desktop (this is also scene 1.1's
  seed). Count instructions from reset to the M1 frame and per idle minute.
  Repeat for Windows 10 when its ISO exists.

  ```sh
  S=tmp/modern/win7-seed.vhd
  qemu-img create -f vpc -o subformat=dynamic $S 24G
  QW="qemu-system-i386 -M pc -accel tcg -cpu pentium2 -m 1024 -display none \
      -vnc 127.0.0.1:5 -monitor unix:tmp/modern/0.4/mon.sock,server,nowait \
      -drive file=$S,format=vpc,if=ide,index=0"

  # Install (not counted). unattend.iso carries autounattend.xml: wipe disk
  # 0, one NTFS partition, image index 1, accept the licence, one
  # administrator with an empty password, auto-logon, no key.
  $QW -boot d \
    -drive file=tmp/modern/media/win7.iso,media=cdrom,if=ide,index=2 \
    -drive file=tmp/modern/media/unattend.iso,media=cdrom,if=ide,index=3
  # Then, in the guest: plain wallpaper, hide the taskbar clock, shut down.
  sha256sum $S > $S.sha256          # the seed is frozen from here

  # Census run 1: quit on the monitor when the M1 desktop frame appears.
  # -snapshot discards guest writes; the plugin prints the total at exit.
  $QW -snapshot -plugin tmp/modern/tools/libinsn.so -d plugin \
      -D tmp/modern/0.4/win7-boot.log
  # Census run 2: the same, quitting 5 minutes of wall time later. The
  # difference over those minutes is the idle rate.
  $QW -snapshot -plugin tmp/modern/tools/libinsn.so -d plugin \
      -D tmp/modern/0.4/win7-idle.log
  grep 'total insns' tmp/modern/0.4/win7-*.log
  ```
  The plugin counts every executed guest instruction, including REP
  iterations as QEMU translates them; scene 1.4 calibrates that against
  r86's step count on the same boot.
- EVIDENCE: `census.txt`: instructions to desktop, idle rate, QEMU command.
- EXITS: ► 1.2 (the seed already exists).

## ACT 1. Windows 7 on Retro

No `--cpu-model`, no `--apic`: Retro, the Pentium II, PIC, ACPI machine as
it is.

### Scene 1.1: the seed

- Made in scene 0.4 under QEMU: 24 GiB dynamic VHD, 1024 MiB RAM,
  auto-logon, plain wallpaper, taskbar clock hidden, updates and indexing
  left at defaults (they are part of the real load).
- Unattended setup uses an `autounattend.xml` on a second small ISO.
- Record `win7-seed.vhd` SHA256. From here it is opened only frozen.

### Scene 1.2: first boot in r86

- ACTION, on the x86 host, interpreter:

  ```sh
  sha256sum -c tmp/modern/win7-seed.vhd.sha256
  $R $FW --hdd tmp/modern/win7-seed.vhd --hdd-frozen --no-cdrom \
    --mem-mib 1024 --usb-tablet --ws-listen 127.0.0.1:8790 \
    --do-not-exit-when-poweroff --debug-ring &
  U=ws://127.0.0.1:8790
  python3 script/ws_cli.py --url $U status
  python3 script/ws_cli.py --url $U frame tmp/modern/1.2/first.ppm
  python3 script/ws_cli.py --url $U logs --follow > tmp/modern/1.2/events.log &
  ```
  The guest re-detects hardware once; that run is not timed. Frozen writes
  die with the process, so every later run re-detects too: M1 is therefore
  taken on the second guest boot inside one process (guest restart, not
  process restart), the same way for every engine and host.
- Checks A2 and A7: Device Manager shows the PIC ACPI HAL ("ACPI x86-based
  PC") and the standard IDE controller.
- Checks A12: with the built-in display driver, Screen Resolution lists the
  VBE modes and a change to 1024x768 in 32-bit colour takes effect. The
  desktop frame recorded for M1 is taken at that mode.
- EXITS:
  - desktop ► 1.4;
  - any table B symptom ► 1.3;
  - desktop, but the HAL is not the PIC one ► record; A2 is false; ► 1.4.

### Scene 1.3: blocker loop

Table B, budget 5. Each fix names a KNOWN_ISSUES entry it adds or removes.

- EXITS: fixed ► 1.2. Budget spent ► G1 as "blocked".

### Scene 1.4: measure

- ACTION: M1-M3 on x86 interpreter, RPi 5 interpreter and JIT, then the Mac
  run by the USER. M4 once on the RPi 5 JIT.

  ```sh
  # Timing binaries (no profile feature) and the M4 binary, kept apart.
  RUSTFLAGS='-C linker=rust-lld' CARGO_TARGET_DIR=tmp/target-intern \
    cargo build --locked --release --features jit \
    --target aarch64-unknown-linux-musl --bin r86-run
  RUSTFLAGS='-C linker=rust-lld' CARGO_TARGET_DIR=tmp/target-intern/profile \
    cargo build --locked --release --features jit,jit-profile \
    --target aarch64-unknown-linux-musl --bin r86-run

  # M1 endpoint and M2 latencies, sketched; DESKTOP is the hash recorded
  # in scene 1.2 from the settled desktop (ws_cli.py's docstring defines
  # the reply fields).
  hash() { python3 script/ws_cli.py --url $U hash-frame; }
  t0=$(date +%s.%N)
  until hash | grep -q "$DESKTOP"; do sleep 0.2; done
  t1=$(date +%s.%N); python3 script/ws_cli.py --url $U status   # steps, CPU
  # M2, Start menu: press and release the Windows key, wait for MENU.
  python3 script/ws_cli.py --url $U keydown MetaLeft
  python3 script/ws_cli.py --url $U keyup MetaLeft
  until hash | grep -q "$MENU"; do sleep 0.05; done
  ```
  Key names are those `script/ws_cli.py` accepts; fix the sketch to them.
  On the RPi 5 every run is `flock /dev/shm/r86-bench.lock taskset -c 2 ...`.
- Also compute the calibration: r86 guest steps to desktop divided by scene
  0.4's QEMU instruction count.
- EVIDENCE: `handoff.txt` with the table; Lead copies it to
  `tmp/perf/scoreboard.txt` as the first Windows 7 and first Apple rows.
- EXITS: ► 1.5.

### Scene 1.5: PAE probe

Purpose: price F3 before any NX work makes PAE the default (A4).

- ACTION: in one frozen process, run `bcdedit /set pae ForceEnable`, reboot
  the guest (frozen writes survive a guest reboot), then M1 and M2 on the
  RPi 5 JIT and interpreter. Compare with 1.4.

  ```bat
  rem In an elevated guest prompt, typed through ws_cli.py:
  bcdedit /set {current} pae ForceEnable
  shutdown /r /t 0
  rem After the reboot: confirm the kernel in use.
  wmic os get PAEEnabled
  ```
- EVIDENCE: PAE/non-PAE ratio for M1 and M2; slow page walks per 1M steps.
- EXITS: ► G1. The ratio feeds D3 (interlude I-PAE).

### G1: Windows 7 verdict (USER)

Inputs: 1.4 table against T, 1.5 ratio, open blockers.

- FAIL or blocked ► **L1**.
- MARGINAL ► scene 2.1.
- PASS ► D1.

### Scene 2.1: cost list (only after MARGINAL)

- ACTION: from M4, list the three largest costs with their share of cycles
  and whether fixing each also helps Windows 2000/XP (for example INT/IRET
  round trips, the deadline tail, 16-bit share during boot).
- EXITS: the USER picks an interlude ► the named act 5 scene ► 1.4 ► G1.
  G1 is re-run once; a second MARGINAL is a FAIL.

### D1: Windows 10 forecast (Lead)

forecast M1 = Windows 7 M1 on the Mac x (Windows 10 census / Windows 7
census). Idle forecast uses the idle rates the same way.

- forecast FAILs T by more than 2x ► **L2**.
- otherwise ► D2. The forecast ignores SSE and PAE cost; G2 corrects it.

### D2: act order (Lead)

Act 3 answers the open speed question (PAE, NX and SSE cost on a real
Windows) and act 4 answers none, so act 3 goes first when it can.

- Plan-intern items 4c and 4e have landed, or the USER pauses item 4 ►
  act 3, then act 4.
- Otherwise ► act 4 first (devices only, no interpreter loop edits), then
  act 3 after 4c and 4e.

## ACT 3. Modern CPU model

Owner files: `src/x86/**`. Small hunks in `src/machine.rs`,
`src/frontend/cli.rs`, `src/frontend/config.rs`. References in AGENTS.md
order: `refs/tiny386/simd.inc` and `i386.c` CPUID (BSD, may be ported with
its notice); `refs/v86/src/rust/cpu/sse_instr.rs`, `instructions_0f.rs`
(BSD-2); Bochs and QEMU study only; Intel SDM volume 2 for NaN, conversion
and MXCSR rules. CPUID starting table: QEMU's `coreduo` definition
(`refs/qemu/target/i386/cpu.c:2670`), read as a fact list.

Every scene ends with gate P unless it says otherwise, plus the no_std
machine and wasm cfg builds.

### Scene 3.1: model, CPUID, MSRs, CR4

- `CpuModel` in the core; `--cpu-model pentium2|modern`, config key
  `cpu-model`. No UI, no `vm_store` field.
- Modern CPUID: signature 06E8h (family 6, model 14, stepping 8); leaf 1
  EDX adds SSE, SSE2, CLFSH (CLFLUSH line 8); ECX SSE3 only (no MONITOR);
  leaf 2 and leaf 4 cache descriptors; 80000000h-80000008h with NX in
  80000001h EDX[20], a brand string and 36 physical bits. The APIC bit
  follows contract 1.
- MSRs: EFER (C0000080h) with NXE only; LME and every other bit #GP.
  Unknown MSR indexes are reported through the existing debug ring during
  bring-up; the #GP itself stays.
- CR4: the modern mask adds OSXMMEXCPT. The Pentium II mask is unchanged.

The model is cold state, appended to `Cpu` (gate P), set before the first
reset and never changed while the guest runs:

```rust
// src/x86/cpu.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuModel {
    /// Family 6 model 5: today's CPUID, MSRs and CR4 mask.
    Pentium2,
    /// Family 6 model 14: adds SSE, SSE2, SSE3 and NX through EFER.
    Modern,
}

pub struct Cpu {
    // ... existing fields, unchanged order ...
    pub model: CpuModel,
    /// A local APIC is attached (scene 4.1). CPUID's APIC bit is this and
    /// IA32_APIC_BASE bit 11; without a device the MSR stays plain storage.
    pub apic: bool,
    pub msr_efer: u64,
}
```

CPUID keeps today's function for the Pentium II and adds one table. Today
`cpuid_leaf(eax, _ecx)` takes no CPU (`interp.rs:4131`):

```rust
const CPUID_FEATURE_EDX_APIC: u32 = 1 << 9;
const CPUID_FEATURE_EDX_CLFSH: u32 = 1 << 19;
const CPUID_FEATURE_EDX_SSE: u32 = 1 << 25;
const CPUID_FEATURE_EDX_SSE2: u32 = 1 << 26;
const CPUID_FEATURE_ECX_SSE3: u32 = 1 << 0;
const CPUID_EXT_EDX_NX: u32 = 1 << 20;
const MODERN_SIGNATURE: u32 = 0x0000_06e8;
const MODERN_FEATURES_EDX: u32 =
    CPUID_FEATURES_EDX | CPUID_FEATURE_EDX_CLFSH | CPUID_FEATURE_EDX_SSE | CPUID_FEATURE_EDX_SSE2;

fn cpuid_leaf(cpu: &Cpu, eax: u32, ecx: u32) -> CpuidLeaf {
    let apic = if cpu.apic && cpu.msr_apic_base & APIC_BASE_ENABLE != 0 {
        CPUID_FEATURE_EDX_APIC
    } else {
        0
    };
    if cpu.model == CpuModel::Pentium2 {
        let mut leaf = pentium2_cpuid_leaf(eax); // today's match, unchanged
        if eax == 1 {
            leaf.edx |= apic;
        }
        return leaf;
    }
    match eax {
        0x4000_0000 => CpuidLeaf::new(0x4000_0000, 0x7236_3872, 0x3872_3638, 0x3638_7236),
        0x0000_0000 => CpuidLeaf::new(4, CPUID_VENDOR_EBX, CPUID_VENDOR_ECX, CPUID_VENDOR_EDX),
        // EBX[15:8]: CLFLUSH line size in quadwords (64 bytes).
        0x0000_0001 => CpuidLeaf::new(
            MODERN_SIGNATURE,
            0x0000_0800,
            CPUID_FEATURE_ECX_SSE3,
            MODERN_FEATURES_EDX | apic,
        ),
        0x0000_0002 => modern_cache_descriptors(),
        0x0000_0004 => modern_cache_parameters(ecx), // one core, one thread
        0x8000_0000 => CpuidLeaf::new(0x8000_0008, 0, 0, 0),
        0x8000_0001 => CpuidLeaf::new(0, 0, 0, CPUID_EXT_EDX_NX),
        0x8000_0002..=0x8000_0004 => modern_brand_string(eax),
        0x8000_0006 => modern_l2_cache(),
        // 36 physical bits (PC_PHYS_LIMIT in machine.rs), 32 linear.
        0x8000_0008 => CpuidLeaf::new(0x0000_2024, 0, 0, 0),
        _ => CpuidLeaf::zero(),
    }
}
```

Values for leaves 2, 4, 80000002h-80000006h come from QEMU's `coreduo`
entry and a real Core Duo dump, recorded in the handoff. Max basic leaf is
4, not QEMU's 10: leaves 5-10 (MONITOR, thermal, perfmon) are not modelled.

EFER needs the index fix of F18 first. Without it C0000080h reads MSR 80h:

```rust
const MSR_EFER: u32 = 0xc000_0080;
const EFER_NXE: u64 = 1 << 11;

fn msr_index(cpu: &Cpu, ecx: u32) -> u32 {
    match cpu.model {
        CpuModel::Pentium2 => canonical_p6_msr_index(ecx), // unchanged
        CpuModel::Modern => ecx,
    }
}

// rdmsr
MSR_EFER if cpu.model == CpuModel::Modern => cpu.msr_efer,

// wrmsr: NXE is the only implemented bit; SCE, LME and the rest fault.
MSR_EFER if cpu.model == CpuModel::Modern => {
    if value & !EFER_NXE != 0 {
        return Err(StepError::GeneralProtection { eip, code: 0 });
    }
    let changed = cpu.msr_efer ^ value;
    cpu.msr_efer = value;
    if changed & EFER_NXE != 0 {
        // Same reach as a CR4.PAE change (after_translation_context_change,
        // interp.rs:11436): CPU caches and the bus-side JIT state.
        cpu.invalidate_translation_context();
        bus.after_control_register_write(4, cpu.cr4 ^ CR4_PAE, cpu.cr4);
    }
}
```

The last line reuses the existing bus hook by presenting the change as a
PAE toggle. If the hook's implementation looks at more than "mapping
changed", add an explicit hook instead; do not leave the JIT link planes
alive across an NXE change.

CR4 (`interp.rs:11494` masks with `CR4_SUPPORTED_MASK`):

```rust
pub const CR4_OSXMMEXCPT: u32 = 1 << 10;

fn cr4_supported_mask(cpu: &Cpu) -> u32 {
    match cpu.model {
        CpuModel::Pentium2 => CR4_SUPPORTED_MASK,
        CpuModel::Modern => CR4_SUPPORTED_MASK | CR4_OSXMMEXCPT,
    }
}
```

Frontend plumbing follows `--usb-tablet` (`cli.rs:980`, key list in
`config.rs:20`):

```rust
// src/frontend/cli.rs, argument loop
"--cpu-model" => {
    cpu_model = match take_value(&mut args, "--cpu-model")?.as_str() {
        "pentium2" => CpuModel::Pentium2,
        "modern" => CpuModel::Modern,
        other => return Err(format!("--cpu-model expects pentium2 or modern, got {other}")),
    }
}
// after Machine construction, before the first step
machine.set_cpu_model(args.cpu_model);
```

- Checks: CPUID and MSR unit tests for both models; a test that the
  Pentium II still aliases MSR indexes exactly as before; the usage text
  and `design/CONFIGURATION.md` name the option.
- EXITS: ► 3.2.

### Scene 3.2: NX

- Contract: with CR4.PAE and EFER.NXE, bit 63 of a PDE or PTE forbids
  instruction fetch; the fault has PFEC bit 4. With NXE clear, bit 63 is
  reserved (PFEC bit 3). PDPTE bit 63 stays reserved. A WRMSR that changes
  NXE discards the translation and fetch caches and both JIT link planes,
  like a CR4.PAE change.
- NX is resolved when the fetch cache is filled: an NX page never enters it,
  so no per-instruction check is added. Data translations are unaffected.
- The comment at `src/x86/mmu.rs:470` ("non-PAE execution has the same page
  permissions as a read") marks the one place a later PAE resident path
  must add the NX test.
- F4 is fixed here for both models (contract 2); delete debt CPU-2.

All of it sits in the PAE walker and `page_fault`; the non-PAE walker and
every cache-hit path are untouched. Today's reserved masks are constants
(`mmu.rs:242-245`) and include bit 63:

```rust
// src/x86/mmu.rs
const PAE_NX: u64 = 1 << 63;

/// Bits 63:36 are reserved without EFER.NXE; with it, bit 63 is NX in PDEs
/// and PTEs. PDPTEs keep PAE_PDPTE_RESERVED_MASK in both cases.
#[inline]
fn pae_nx_as_reserved(cpu: &Cpu) -> u64 {
    if cpu.msr_efer & EFER_NXE != 0 { 0 } else { PAE_NX }
}

// walk_pae, replacing the two PAE_ENTRY_RESERVED_MASK tests (4 KiB path):
let reserved = (PAE_ENTRY_RESERVED_MASK & !PAE_NX) | pae_nx_as_reserved(cpu);
if (pde & reserved) != 0 {
    return page_fault(cpu, linear, access, priv_, true, true);
}
// ... PTE read ...
if (pte & reserved) != 0 {
    return page_fault(cpu, linear, access, priv_, true, true);
}
// User and write rights AND across the levels; NX ORs across them. With
// NXE clear a set bit 63 already faulted as reserved, so no NXE test here.
check_perm_pae(cpu, linear, access, priv_, pde & pte, (pde | pte) & PAE_NX != 0, true)?;

// check_perm_pae gains `nx: bool`, tested with the other rights and before
// any accessed/dirty update:
if nx && matches!(access, AccessKind::Execute) {
    return page_fault(cpu, linear, access, priv_, present, false);
}

// translate_pae_2m: the same two changes with PAE_PDE_2M_RESERVED_MASK and
// the PDE alone.

// page_fault (mmu.rs:1949), both models: the bit exists only with NX on.
if matches!(access, AccessKind::Execute)
    && cpu.cr4 & CR4_PAE != 0
    && cpu.msr_efer & EFER_NXE != 0
{
    code |= PFEC_INSTR;
}
```

Why no fetch-path code is needed: the fetch cache is filled only after a
successful `AccessKind::Execute` walk, and the JIT enters a block only
through a valid fetch-cache entry (`runtime.rs:712`, 755). An NX page
faults in the walk and so never reaches either. The resident shortcut
(`try_resident_fetch_translation`, `mmu.rs:478`) returns `None` under PAE
(F3), so it cannot bypass the walk today. Debt CPU-2 records that the
tests no longer assert PFEC bit 4 on execute faults; this scene adds both
the positive case (PAE with NXE) and the negative ones (non-PAE; PAE
without NXE).
- Checks: fetch fault and error code; NXE-clear reserved-bit fault; 2 MiB
  page; a JIT block whose page becomes NX is not entered; A9: boot TinyCore
  and read whether its kernel reports NX.
- EXITS: gate P holds ► 3.3. A9 true ► NX's first real guest is Windows in
  scene 3.8; say so in the handoff.

### Scene 3.3: FXSAVE, FXRSTOR, MXCSR

- XMM0-7 and MXCSR in the FXSAVE image; on the modern model MXCSR_MASK is
  FFFFh (DAZ supported); reserved MXCSR bits #GP on LDMXCSR and FXRSTOR.
  The Pentium II keeps today's 160-byte image and FFBFh mask (F20).
- LDMXCSR/STMXCSR follow the SSE gating of scene 3.4 on the modern model
  (closes that part of debt CPU-3). Their Pentium II behaviour does not
  move (contract 2); the debt entry stays recorded for that model.

XMM state is two 64-bit halves per register, so integer SSE2 can reuse the
MMX lane functions (scene 3.4). It is appended to `FpuState`
(`cpu.rs:100`):

```rust
pub struct FpuState {
    // ... existing fields ...
    /// XMM0-7, little-endian halves: [bits 63:0, bits 127:64].
    pub xmm: [[u64; 2]; 8],
}
```

`fxsave`/`fxrstor` (`interp.rs:12520`, 12540) grow by model. The image is
built on the stack and written once, so a fault leaves memory unchanged,
as today:

```rust
const FXSAVE_XMM_IMAGE_LEN: usize = 288; // through XMM7; 288..512 untouched
const MODERN_MXCSR_MASK: u32 = 0x0000_ffff;

// fxsave, modern model
let mut image = [0u8; FXSAVE_XMM_IMAGE_LEN];
// ... bytes 0..160 exactly as today ...
image[24..28].copy_from_slice(&cpu.fpu.mxcsr.to_le_bytes());
image[28..32].copy_from_slice(&MODERN_MXCSR_MASK.to_le_bytes());
for (index, xmm) in cpu.fpu.xmm.iter().enumerate() {
    let offset = FXSAVE_XMM_REG_BASE + index * 16;
    image[offset..offset + 8].copy_from_slice(&xmm[0].to_le_bytes());
    image[offset + 8..offset + 16].copy_from_slice(&xmm[1].to_le_bytes());
}
write_mem_image_at(cpu, bus, addr, &image, FXSAVE_XMM_IMAGE_LEN, eip)

// fxrstor, modern model: validate before any state changes.
let mxcsr = u32::from_le_bytes(image[24..28].try_into().unwrap());
if mxcsr & !MODERN_MXCSR_MASK != 0 {
    return Err(StepError::GeneralProtection { eip, code: 0 });
}
```

Hardware saves XMM and MXCSR only when CR4.OSFXSR is set; with it clear
the choice is implementation-defined. r86 saves them whenever the model is
modern; the differential harness does not cover this case.
- Debt FPU-3 (tag word, MMX restore guess) is fixed only if a guest symptom
  appears in 3.8 or 6.3; otherwise it stays listed as open.
- EXITS: ► 3.4.

### Scene 3.4: SSE and SSE2, moves and integer

- New module, one cold entry from `execute_0f` (contract 4). The mandatory
  prefix selects the operation; `reject_sse_prefixes` becomes a model test.
- Gating order: #UD when the model lacks the instruction, CR0.EM is set or
  CR4.OSFXSR is clear; #NM when CR0.TS is set; #GP(0) for a misaligned
  aligned-form memory operand.
- Scope: MOVAPS/MOVUPS/MOVDQA/MOVDQU/MOVQ/MOVD/MOVSS/MOVSD/MOVLPS/MOVHPS and
  their double forms, MOVNTx, MASKMOVDQU, shuffles and unpacks, all XMM
  integer operations (sharing MMX's lane helpers), the MMX-register
  additions of SSE and SSE2 (PSHUFW, PAVGB, PMINUB, PMAXUB, PEXTRW, PINSRW,
  PMOVMSKB, PMULHUW, PSADBW, MOVNTQ, MASKMOVQ, PADDQ, PSUBQ, PMULUDQ),
  MOVNTI. CLFLUSH, the fences and PREFETCHh already execute (F20).

**Entry.** No test is added in front of the two-byte dispatch: SSE-only
opcodes get their own match arms (a jump-table target each, like every
other arm), and the MMX arms turn their existing `reject_sse_prefixes`
call into the branch to SSE. Opcodes that are neither pay nothing.

```rust
// src/x86/interp.rs, execute_0f (already #[inline(never)], F19)

// Rows that exist only as SSE: MOVUPS..MOVHPS, MOVAPS..COMISS, the
// 50h-5Fh arithmetic row, PSHUFx, CMPPS, PINSRW/PEXTRW/SHUFPS, ...
0x10..=0x17 | 0x28..=0x2f | 0x50..=0x5f | 0x70 | 0xc2 | 0xc4..=0xc6 /* ... */ => {
    sse::execute(cpu, bus, prefix, operand_size_prefix, opcode2, address16, start_eip)?
}

// MMX arms, today:
//     reject_sse_prefixes(prefix, operand_size_prefix, start_eip)?;
// become:
0x60..=0x6b | 0x74..=0x76 /* ... the existing MMX list ... */ => {
    if operand_size_prefix || !matches!(prefix, Prefix::None) {
        return sse::execute(cpu, bus, prefix, operand_size_prefix, opcode2, address16, start_eip);
    }
    let modrm = fetch_u8(cpu, bus)?;
    mmx_binary_op(cpu, bus, opcode2, modrm, address16, start_eip)?;
}
```

`0F C3` (MOVNTI) leaves the `0x38 | 0x3a | 0xc3 => #UD` arm at
`interp.rs:3068`; 38h and 3Ah (SSSE3, SSE4) stay #UD on both models.

**Gating**, in the new module, once per instruction:

```rust
// src/x86/sse.rs
#[inline(never)]
pub(super) fn execute<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    prefix: Prefix,
    operand_size_prefix: bool,
    opcode2: u8,
    address16: bool,
    eip: u32,
) -> Result<(), StepError> {
    // #UD outranks #NM (SDM vol. 2, exception classes of the SSE tables).
    // On the Pentium II this is the whole function: today's #UD.
    if cpu.model != CpuModel::Modern
        || cpu.cr0 & CR0_EM != 0
        || (cpu.cr4 & CR4_OSFXSR == 0 && !mmx_register_form(prefix, operand_size_prefix, opcode2))
    {
        return Err(StepError::InvalidOpcode { eip });
    }
    if cpu.cr0 & CR0_TS != 0 {
        return Err(StepError::DeviceNotAvailable { eip });
    }
    let form = match (operand_size_prefix, prefix) {
        (false, Prefix::None) => Form::Ps,   // packed single, or MMX register
        (true, Prefix::None) => Form::Pd,    // 66: packed double, or XMM integer
        // F3 and F2 select the form even when 66 is also present
        // (confirm against Bochs fetchdecode32 in this scene).
        (_, Prefix::Rep) => Form::Ss,
        (_, Prefix::Repne) => Form::Sd,
    };
    // ... one match on (opcode2, form) ...
}

/// A 16-byte memory operand of an aligned form (MOVAPS, MOVDQA, and every
/// packed operation's m128 source) must be 16-byte aligned: #GP(0).
fn aligned_m128(addr: MemAddr, eip: u32) -> Result<MemAddr, StepError> {
    // Tests the segment offset, as aligned_fxsave_memory does today; the
    // linear-address form is KNOWN_ISSUES' existing FXSAVE entry. Fix both
    // together or neither.
    if addr.offset & 0x0f != 0 {
        return Err(StepError::GeneralProtection { eip, code: 0 });
    }
    Ok(addr)
}
```

The MMX-register additions (PSHUFW, PAVGB and the others listed above) do
not need CR4.OSFXSR; they gate like MMX (`mmx_available`,
`interp.rs:8782`). That is what `mmx_register_form` separates.

**Integer SSE2 on the MMX lane functions.** `mmx_binary_result`
(`interp.rs:8890`) is `pub(super)` and pure. Lane-wise operations apply it
to each half; only three groups cross the halves:

```rust
/// 66-prefixed forms of the MMX binary opcodes on 128-bit operands.
fn sse2_integer_result(opcode: u8, lhs: [u64; 2], rhs: [u64; 2]) -> [u64; 2] {
    match opcode {
        // PUNPCKL*: all 16 bytes come from the two low quadwords. The high
        // half is the MMX PUNPCKH* (opcode + 8) of those same quadwords.
        0x60..=0x62 => [
            mmx_binary_result(opcode, lhs[0], rhs[0]),
            mmx_binary_result(opcode + 8, lhs[0], rhs[0]),
        ],
        // PUNPCKH*: the mirror image, from the two high quadwords.
        0x68..=0x6a => [
            mmx_binary_result(opcode - 8, lhs[1], rhs[1]),
            mmx_binary_result(opcode, lhs[1], rhs[1]),
        ],
        // PACKSSWB, PACKUSWB, PACKSSDW: destination elements, then source.
        0x63 | 0x67 | 0x6b => [
            mmx_binary_result(opcode, lhs[0], lhs[1]),
            mmx_binary_result(opcode, rhs[0], rhs[1]),
        ],
        // Shifts by a register count: one count, the low quadword.
        0xd1..=0xd3 | 0xe1 | 0xe2 | 0xf1..=0xf3 => [
            mmx_binary_result(opcode, lhs[0], rhs[0]),
            mmx_binary_result(opcode, lhs[1], rhs[0]),
        ],
        // Everything else in the MMX list is lane-wise.
        _ => [
            mmx_binary_result(opcode, lhs[0], rhs[0]),
            mmx_binary_result(opcode, lhs[1], rhs[1]),
        ],
    }
}
```

PUNPCKLQDQ/PUNPCKHQDQ (6Ch, 6Dh), PSHUFD/PSHUFLW/PSHUFHW, PSLLDQ/PSRLDQ,
PMULUDQ, PADDQ/PSUBQ, PAVGx, PMINx/PMAXx, PSADBW and PMOVMSKB have no MMX
P6 counterpart in the tree and are written new, once, for both widths.

**Differential harness** (starts here, grows in 3.5 and 3.6). The x86_64
host CPU is the oracle; register forms with ModRM C1h (`xmm0, xmm1`) have
the same encoding in 64-bit mode:

```rust
// tests/sse_diff.rs
#[cfg(target_arch = "x86_64")]
fn native(stub: *const u8, a: &mut [u64; 2], b: &[u64; 2], mxcsr: &mut u32) {
    // `stub` is an executable page holding the guest bytes followed by RET.
    let mut host_mxcsr = 0u32;
    unsafe {
        core::arch::asm!(
            "stmxcsr [{host}]",
            "ldmxcsr [{csr}]",
            "movdqu xmm0, [{a}]",
            "movdqu xmm1, [{b}]",
            "call {stub}",
            "movdqu [{a}], xmm0",
            "stmxcsr [{csr}]",
            // Rust code assumes the default MXCSR: restore it before
            // any Rust floating point can run.
            "ldmxcsr [{host}]",
            host = in(reg) &mut host_mxcsr as *mut u32,
            csr = in(reg) mxcsr as *mut u32,
            a = in(reg) a.as_mut_ptr(),
            b = in(reg) b.as_ptr(),
            stub = in(reg) stub,
            out("xmm0") _, out("xmm1") _,
            clobber_abi("C"),
        );
    }
}

/// One record of tests/data/sse_golden.bin, little-endian:
/// len u8, guest bytes [u8; len], mxcsr_in u32, a [u64; 2], b [u64; 2],
/// result [u64; 2], mxcsr_out u32.
/// x86_64: generate from `native` and compare with the interpreter.
/// Every host: replay the file through the interpreter and compare.
```

Vectors per floating operand: +0, -0, 1, -1, the smallest and largest
normals, the smallest and largest denormals, both infinities, a QNaN and an
SNaN with distinct payloads and both signs, 2^31, 2^31 - 1, -2^31 - 1,
2^63, 0.5, 1.5, 2.5 (ties), and 64 values from a fixed-seed generator.
Integer operations use the lane boundaries (0, 1, 7Fh, 80h, FFh and their
word and dword equivalents) plus the same generator.

- Checks: the harness above; results and MXCSR compared. The golden file
  stays under 1 MiB and is replayed on the RPi 5.
- EXITS: ► 3.5.

### Scene 3.5: SSE and SSE2, floating point

- Scalar and packed arithmetic, SQRT, RCP/RSQRT (document the table used),
  MIN/MAX, compares with all predicates, COMISS/UCOMISS and double forms,
  every conversion, under contract 5.

The shape every arithmetic operation follows. The host operation is used
only where every host returns the same bits (no NaN in, no NaN out, round
to nearest); the cases that differ between hosts are decided first:

```rust
const MXCSR_IE: u32 = 1 << 0; // invalid
const MXCSR_DE: u32 = 1 << 1; // denormal operand
const MXCSR_ZE: u32 = 1 << 2; // divide by zero
const MXCSR_OE: u32 = 1 << 3;
const MXCSR_UE: u32 = 1 << 4;
const MXCSR_DAZ: u32 = 1 << 6;
const MXCSR_RC: u32 = 3 << 13; // 0 nearest, 1 down, 2 up, 3 toward zero
const MXCSR_FTZ: u32 = 1 << 15;
const F32_QUIET: u32 = 0x0040_0000;
const F32_INDEFINITE: u32 = 0xffc0_0000; // x86's default NaN is negative

fn is_snan32(bits: u32) -> bool {
    f32::from_bits(bits).is_nan() && bits & F32_QUIET == 0
}

/// ADDSS/SUBSS/MULSS/DIVSS and each lane of the packed forms.
fn sse_arith_f32(mxcsr: &mut u32, a: u32, b: u32, op: impl Fn(f32, f32) -> f32) -> u32 {
    let (a, b) = (daz32(*mxcsr, a), daz32(*mxcsr, b));
    let (fa, fb) = (f32::from_bits(a), f32::from_bits(b));
    // x86 SSE rule: the first operand if it is a NaN, else the second,
    // quieted. ARM picks by signalling first, RISC-V and wasm return a
    // canonical NaN: never let the host choose.
    if fa.is_nan() || fb.is_nan() {
        if is_snan32(a) || is_snan32(b) {
            *mxcsr |= MXCSR_IE;
        }
        return if fa.is_nan() { a | F32_QUIET } else { b | F32_QUIET };
    }
    if fa.is_subnormal() || fb.is_subnormal() {
        *mxcsr |= MXCSR_DE;
    }
    if *mxcsr & MXCSR_RC != 0 {
        return sse_arith_f32_directed(mxcsr, fa, fb, op); // slow, exact
    }
    let r = op(fa, fb);
    if r.is_nan() {
        // inf - inf, 0 * inf, 0 / 0, inf / inf. ARM would give +NaN.
        *mxcsr |= MXCSR_IE;
        return F32_INDEFINITE;
    }
    // ZE (finite / 0), OE and UE by classifying operands and result, then
    // flush-to-zero when FTZ is set and the result is tiny. PE is not
    // tracked (contract 5).
    finish_f32(mxcsr, fa, fb, r)
}

/// MINSS and each lane of MINPS: not IEEE minimum. Any NaN, and equal
/// operands including +0 against -0, return the second operand unchanged.
fn sse_min_f32(mxcsr: &mut u32, a: u32, b: u32) -> u32 {
    let (fa, fb) = (f32::from_bits(daz32(*mxcsr, a)), f32::from_bits(daz32(*mxcsr, b)));
    if fa.is_nan() || fb.is_nan() {
        *mxcsr |= MXCSR_IE; // quiet NaNs raise it here too
        return b;
    }
    if fa < fb { a } else { b }
}

/// CVTTSS2SI r32. Rust's `as` saturates and maps NaN to 0; x86 returns the
/// integer indefinite for both, as fpu_round_i32_indefinite already does.
fn sse_cvtt_f32_i32(mxcsr: &mut u32, a: u32) -> u32 {
    let f = f32::from_bits(a);
    if !(-2_147_483_648.0..2_147_483_648.0).contains(&f) {
        *mxcsr |= MXCSR_IE;
        return 0x8000_0000;
    }
    f as i32 as u32
}
```

The directed-rounding path (`sse_arith_f32_directed`) is one integer
routine: unpack sign, exponent and significand, operate on `u64`/`u128`
significands, round once in the requested direction, repack. It is generic
over the two widths and covers add, subtract, multiply, divide and square
root. Do not derive a directed result from the host's round-to-nearest
result: the bits the host discarded decide the direction. `refs/tiny386`
has no such path (F17); Bochs' softfloat is study only. If the M4 profile
of scenes 3.8 and 6.4 never sees a non-default rounding mode, say so in
the handoff: the path stays, because wrong results are not an option, but
it earns no optimization.

In `sse_min_f32`, whether DAZ changes the bits of a forwarded denormal
operand is settled by the harness, not by this sketch.

Conversions that round (CVTSS2SI, CVTPS2DQ, CVTSD2SS and the rest) apply
MXCSR.RC themselves; the x87 helper `fpu_round` (`interp.rs:13446`) shows
the existing idiom for the four modes.
- Checks: the harness adds NaN, SNaN, denormal, infinity, out-of-range and
  all four rounding modes. PE is masked out of the MXCSR comparison
  (contract 5) and that mask is the only one.
- EXITS: ► 3.6.

### Scene 3.6: SSE3

- ADDSUBPS/PD, HADDPS/PD, HSUBPS/PD, MOVDDUP, MOVSLDUP, MOVSHDUP, LDDQU,
  FISTTP. MONITOR and MWAIT stay #UD and unreported.
- Skipped, with the model's ECX bit cleared, if scene 0.3 run 4 showed
  Windows 10 does not need SSE3 and G4 is not planned.
- EXITS: ► 3.7.

### Scene 3.7: JIT decoder audit

- Every 66/F2/F3-prefixed two-byte form that is not compiled today is a
  coverage miss ending the block at a precise boundary, on both models.
  Extend the existing test at `src/x86/jit/decode.rs:1229` from MMX to the
  whole two-byte map. No native SSE lowering in this act.

  ```rust
  // src/x86/jit/decode.rs, beside mmx_rejects_sse_prefixes_...
  #[test]
  fn sse_forms_end_the_block_before_the_instruction() {
      // Rows whose every form is MMX or SSE.
      let simd_rows = (0x10..=0x17).chain(0x28..=0x2f).chain(0x50..=0x7f)
          .chain([0xc2, 0xc4, 0xc5, 0xc6]).chain(0xd0..=0xff);
      for opcode in simd_rows {
          for prefix in [0x66, 0xf2, 0xf3] {
              let bytes = [prefix, 0x0f, opcode, 0xc1, 0x00];
              assert!(
                  block(&bytes, 0, 1).instructions.is_empty(),
                  "{prefix:02x} 0f {opcode:02x}"
              );
          }
      }
      // SSE-only rows stay misses without a prefix too; MOVNTI likewise.
      for opcode in (0x10..=0x17).chain(0x28..=0x2f).chain(0x50..=0x5f)
          .chain([0xc2, 0xc3, 0xc6])
      {
          assert!(block(&[0x0f, opcode, 0xc1, 0x00], 0, 1).instructions.is_empty());
      }
  }
  ```
  The unprefixed MMX forms the JIT compiles today (`Op::Mmx`,
  `decode.rs:128`) stay compiled. `0F 77` (EMMS) and the 71h-73h shift
  groups are in the 50h-7Fh row: with a prefix they are misses, as the
  existing test already requires.
- Checks: decoder test; one ReactOS or TinyCore JIT boot on the RPi 5 with
  `--cpu-model modern`.
- EXITS: ► 3.8.

### Scene 3.8: guests, and Modern versus Retro

- ACTION:
  1. TinyCore to a shell under both models.
  2. Windows 2000 frozen seed under `pentium2`: unchanged frames and steps.
  3. Windows 7 frozen seed under `modern`: note the kernel chosen (A4), then
     M1, M2 and M4 on the RPi 5 JIT and interpreter. M4 must report the
     share of interpreted entries that are SSE opcodes and the slow page
     walks.
- EVIDENCE: Modern/Retro ratio for M1 and M2; the SSE opcode histogram.
- EXITS: ► G2.

### G2: modern penalty (USER, on Lead's summary)

- ratio at most 1.10 ► continue (act 4, or act 6 if act 4 is done).
- ratio up to 2.0, or above it with one cost of 30% of cycles or more ► D3.
- ratio above 2.0 with no single cost of 30% or more ► **L2b**.

## ACT 5. JIT interludes (only when D3 or scene 2.1 names one)

Each interlude is one pilot; a pilot that does not move its metric by the
stated amount is reverted. Confirmation is two matched runs.

| Id | Enter when | Work | Keep if |
|---|---|---|---|
| I-PAE | 1.5 or 3.8: PAE costs 10% or more of M1 or M2, or slow walks are 5% or more of cycles | PAE in `resident_page` (64-bit entries, PDPTE cache, 2 MiB pages, NX on fetch) and the matching native refill | the PAE ratio falls below 1.05 |
| I-SSE | 3.8 or 6.4: SSE opcodes are 5% or more of cycles | Helper-call lowering in the style of `execute_mmx` (`src/x86/jit/runtime.rs:267`) for the opcodes at the top of the histogram, moves first | interpreted SSE share halves and M2 improves |
| I-TPR | 4.3: TPR writes cost 3% or more of cycles | TPR write handled without leaving the block | the cost falls below 1% |
| I-shared | 2.1 names it | The deferred "Windows JIT fallbacks" item of `tmp/refactor/plan-intern.txt` | per that brief |

I-PAE, where the work is. `resident_page` (`mmu.rs:508`) is the one proof
shared by data, execute and native-fault refills; today it starts with
`if cpu.cr4 & CR4_PAE != 0 { return None; }`:

```rust
// PAE arm of resident_page. Reads only stable RAM words, as the non-PAE
// arm does; anything unusual returns None and the walker decides.
let pdpte = cpu.pae_pdptrs[(linear.0 >> 30) as usize]; // cached at CR3 load
if pdpte & PAE_PTE_PRESENT == 0 {
    return None;
}
let pde_addr = PhysAddr((pdpte & PAE_4K_PAGE_MASK) + u64::from((linear.0 >> 21) & 0x1ff) * 8);
let root = bus.jit_page_table_source(pde_addr)?;
let pde = bus.read_backing_token_u64(root); // new: 64-bit sibling of ..._u32
let present_accessed = PAE_PTE_PRESENT | PAE_PTE_ACCESSED;
if pde & present_accessed != present_accessed || pde & PAE_PDE_PS != 0 {
    return None; // 2 MiB pages: a second step, only if the profile asks
}
let pte_addr = PhysAddr((pde & PAE_4K_PAGE_MASK) + u64::from((linear.0 >> 12) & 0x1ff) * 8);
let leaf = bus.jit_page_table_source(pte_addr)?;
let pte = bus.read_backing_token_u64(leaf);
// Reserved bits, user and write rights exactly as walk_pae; then:
if execute && (pde | pte) & PAE_NX != 0 {
    return None; // the walker raises the fault with the right error code
}
```

Three things change with it, and each is a way to get this wrong:

1. `ResidentPage.pde/pte`, `Translation.recheck` and `TranslationRecheck`
   hold `u32` words (F21). Under PAE they must hold the full 64 bits, or a
   guest flipping only NX leaves a stale execute translation certified.
2. `try_resident_fetch_translation` computes `walk_root` with the non-PAE
   formula `root - (linear >> 22) * 4` (`mmu.rs:498`). The generated refill
   that consumes it needs a PAE variant, or stays off under PAE while the
   Rust-side resident proof alone is measured first.
3. `resident_page` gains an `execute` argument: the comment at
   `mmu.rs:470` ("the same page permissions as a read") stops being true.

I-SSE follows the MMX precedent: `execute_mmx(view, control, value)`
(`runtime.rs:267`) is called from generated code and shares
`mmx_binary_result` with the interpreter. The SSE helper takes the two
halves and shares `sse2_integer_result` and the move forms the same way.
Start with whatever tops scene 3.8's histogram; native NEON lowering is a
later step that needs its own measurement.

D3 (Lead) picks interludes strictly by measured share, largest first.
After the picked interludes: G2 path, ratio at most 1.25 ► continue,
otherwise ► **L2b**; G3 path ► 6.4 once more.

## ACT 4. APIC platform

Owner files: `src/devices/**`, `src/acpi.rs`, `firmware/acpi/*`, the
interrupt source selection in `src/machine.rs`. A change under `src/x86`
needs Lead's agreement. References: `refs/tiny386` (`pc.c`, `i8259.c`; it
has no APIC), `refs/v86/src/rust/cpu/apic.rs`, `ioapic.rs` and `src/acpi.js`
(BSD-2); Bochs and QEMU study only; Intel SDM volume 3 chapter 10, the
82093AA datasheet, ACPI 1.0b/2.0 MADT.

### Scene 4.1: local APIC, I/O APIC, delivery

- `--apic`, config key `apic`, a Machine setter in the `attach_*` style.
  Off by default (contract 2).
- Local APIC behind the existing FEE00000h region (F10): ID, version,
  TPR/PPR, EOI, LDR/DFR, SVR, ISR/TMR/IRR, ESR, ICR (self IPI; INIT/SIPI
  accepted and ignored for a uniprocessor), LVT timer one-shot and periodic
  on the deterministic machine clock, LINT0 ExtINT (virtual wire: the 8259
  delivers through it), LINT1 NMI. No TSC-deadline, no x2APIC.
- I/O APIC behind FEC00000h: 24 entries, edge and level, remote IRR,
  masking, EOI from the local APIC, IRQ0 on pin 2.
- IA32_APIC_BASE: FEE00900h; writes keep base and BSP and record the enable
  bit. CPUID's APIC bit follows the device.
- Delivery: `deliver_irq` (F11) and `pending_external_irq` take the vector
  from the local APIC when it is software-enabled, from the PIC otherwise.
  The PIC-only path gains at most one branch, outside the per-instruction
  loop; gate P covers it.
- SeaBIOS needs no rebuild (F22): once CPUID reports the APIC it puts the
  local APIC into virtual-wire mode itself and finds one CPU. The device
  must therefore accept, from the first boot: SVR enable, LINT0 = 8700h,
  LINT1 = 8400h, and two ICR writes (C4500h INIT, C46xxh SIPI) that reach
  no other processor.

The device, one file, no CPU or bus references inside it:

```rust
// src/devices/apic.rs
pub(crate) const LAPIC_BASE: u64 = 0xfee0_0000;
pub(crate) const IOAPIC_BASE: u64 = 0xfec0_0000;
pub(crate) const APIC_BASE_RESET: u64 = LAPIC_BASE | (1 << 11) | (1 << 8); // enable, BSP

const SVR_ENABLE: u32 = 1 << 8;
const LVT_MASKED: u32 = 1 << 16;
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
const LVT_DELIVERY_EXTINT: u32 = 7 << 8;

pub(crate) struct Apic {
    // Local APIC
    tpr: u32,
    svr: u32,
    ldr: u32,
    dfr: u32,
    esr: u32,
    icr: [u32; 2],
    lvt_timer: u32,
    lvt_lint: [u32; 2],
    lvt_error: u32,
    isr: [u32; 8],
    tmr: [u32; 8],
    irr: [u32; 8],
    timer_divide: u32,
    timer_initial: u32,
    timer_start: u64, // device tick of the last initial-count write or reload
    // I/O APIC
    io_index: u8,
    io_redirection: [u64; 24],
    io_pins: u32,       // current line levels
    io_remote_irr: u32,
}

impl Apic {
    /// Processor priority: the higher class of TPR and the highest ISR bit.
    fn ppr(&self) -> u32 {
        let in_service = highest_bit(&self.isr).map_or(0, |v| u32::from(v) & 0xf0);
        if self.tpr & 0xf0 >= in_service { self.tpr } else { in_service }
    }

    /// The vector the CPU would take now from the local APIC, if any.
    pub fn pending_vector(&self) -> Option<u8> {
        if self.svr & SVR_ENABLE == 0 {
            return None;
        }
        let vector = highest_bit(&self.irr)?;
        (u32::from(vector) & 0xf0 > self.ppr() & 0xf0).then_some(vector)
    }

    /// LINT0 passes the 8259's INTR through: SeaBIOS's virtual wire, and
    /// what DOS, Windows 2000 and every PIC HAL keep using. LINT0 resets
    /// to ExtINT, unmasked (700h), as QEMU does for the boot CPU, so PIC
    /// interrupts work before anything programs the APIC; confirm the
    /// software-disabled case against Bochs.
    pub fn extint_open(&self) -> bool {
        self.svr & SVR_ENABLE == 0
            || self.lvt_lint[0] & (LVT_MASKED | (7 << 8)) == LVT_DELIVERY_EXTINT
    }

    /// Divide configuration bits 0, 1 and 3; 111b divides by 1.
    fn timer_divisor(&self) -> u64 {
        let code = (self.timer_divide & 3) | ((self.timer_divide & 8) >> 1);
        if code == 7 { 1 } else { 2 << code }
    }

    /// The APIC bus clock is one device tick (F24): 16 * 1_193_182 Hz.
    /// Guests calibrate it against the PIT or the PM timer, so only its
    /// stability matters, and it is exact on the deterministic clock.
    pub fn next_timer_tick(&self) -> u64 {
        if self.timer_initial == 0 || self.lvt_timer & LVT_MASKED != 0 {
            return u64::MAX;
        }
        // One-shot: start + initial * divisor, once. Periodic: the next
        // multiple of the period after the last serviced tick. Take the
        // period (initial or initial + 1) from Bochs, not from memory.
        self.timer_deadline()
    }

    pub fn memory_read(&mut self, addr: u64, len: u32, tick: u64) -> Option<u32>;
    pub fn memory_write(&mut self, addr: u64, len: u32, value: u32, tick: u64) -> bool;
    pub fn advance(&mut self, tick: u64);          // fire due timer interrupts
    pub fn acknowledge(&mut self) -> u8;           // highest IRR -> ISR
    pub fn set_pin(&mut self, pin: u8, high: bool); // I/O APIC input
}
```

The machine side is four small hunks, each beside the code it extends:

```rust
// src/machine.rs

// HostBus field, like `usb: Option<Uhci>`.
apic: Option<Apic>,

// 1. Interrupt source (today: machine.rs:87 and 123). The `None` arm is
//    today's code; it is the only arm an existing VM ever takes.
fn pending_external_irq(cpu: &Cpu, bus: &HostBus) -> Option<u8> {
    cpu.maskable_interrupts_enabled()
        .then(|| match &bus.apic {
            None => bus.pic.pending_irq(),
            Some(apic) => apic.pending_vector().or_else(|| {
                apic.extint_open().then(|| bus.pic.pending_irq()).flatten()
            }),
        })
        .flatten()
}

fn deliver_irq(cpu: &mut Cpu, bus: &mut HostBus, return_eip: u32) -> Result<(), String> {
    let vector = match bus.apic.as_mut() {
        Some(apic) if apic.pending_vector().is_some() => apic.acknowledge(),
        _ => bus.pic.acknowledge(),
    };
    let result = deliver_external_interrupt(cpu, bus, vector, return_eip);
    finish_event_delivery(cpu, bus, result)
}

// 2. ISA lines (drive_irq, machine.rs:2314): the 8259 and the I/O APIC see
//    the same wire. IRQ0 arrives on pin 2; the cascade (IRQ2) has no pin.
if let Some(apic) = self.apic.as_mut() {
    apic.set_pin(if irq == 0 { 2 } else { irq }, high);
}

// 3. Time (next_device_event, machine.rs:2413, and the scan in
//    refresh_interrupt_attention, 2479), in the existing idiom:
.min(self.apic.as_ref().map_or(u64::MAX, Apic::next_timer_tick))
// ...
if let Some(apic) = self.apic.as_mut() {
    apic.advance(ticks);
}

// 4. MMIO, beside vga.memory_read in the slow read path (machine.rs:3505)
//    and its write twin, before unclaimed_memory:
if let Some(apic) = self.apic.as_mut() {
    if let Some(value) = apic.memory_read(addr.0, 1, ticks) {
        return Ok(value as u8);
    }
}
```

`pending_irq()` returns the PIC's line number and is used here only as
"something is pending"; keep whatever the existing callers expect. The
PIT's replayed edge (`update_pit0_irq`, `machine.rs:2359`) must reach pin 2
as an edge as well. Whether the device needs the CPU's IA32_APIC_BASE
enable bit is settled by reading Bochs; if it does, `attach_apic` and
WRMSR 1Bh pass it in, the device does not reach for the CPU.

```rust
// Attach, in the attach_usb_tablet style (machine.rs:683):
pub fn attach_apic(&mut self) {
    self.bus.apic = Some(Apic::new());
    self.cpu.apic = true;
    self.cpu.msr_apic_base = APIC_BASE_RESET; // also on every CPU reset
    self.bus.set_attention();
}

// wrmsr, with a device attached: base and BSP are fixed.
MSR_APIC_BASE if cpu.apic => {
    cpu.msr_apic_base = APIC_BASE_RESET & !(1 << 11) | (value & (1 << 11));
}
```

- Checks: unit tests for timer modes and divide, TPR/PPR masking, EOI and
  ISR order, remote IRR, masked pins, virtual wire. One machine-level test:
  with the APIC attached and untouched by the guest, a PIT interrupt is
  delivered exactly as without it. Gate P.
- EXITS: ► 4.2.

### Scene 4.2: MADT

- MADT built in `src/acpi.rs` only with `--apic`: local APIC, I/O APIC,
  the IRQ0 override, level/active-high overrides for the PCI lines, LINT1
  NMI.
- **The DSDT does not change.** Its `_PRT` names interrupt numbers 11 and
  14 directly (`dsdt.asl:35`). In APIC mode the same numbers are I/O APIC
  pins, because ISA IRQ n is wired to pin n (IRQ0 to pin 2). What an
  APIC-mode OS lacks is only the trigger and polarity of the shared PCI
  line, and the MADT supplies that, exactly as QEMU's does (F23). No
  `_PIC` method, no second `_PRT`, no AML rebuild. This replaces revision
  1's "APIC-mode `_PRT`".

`firmware_files()` (`src/acpi.rs:154`) builds one blob with fixed offsets
and a SeaBIOS loader script. The MADT is appended after the RSDT and the
RSDT grows by one pointer:

```rust
/// MADT: header 36, local APIC address 4, flags 4, then entries. 100 bytes.
fn madt() -> [u8; 100] {
    let mut t = [0u8; 100];
    header(&mut t, b"APIC");
    put32(&mut t, 36, 0xfee0_0000); // local APIC address
    put32(&mut t, 40, 1);           // PCAT_COMPAT: dual 8259s are present
    // Processor local APIC: processor 0 (the DSDT's CPU0), APIC id 0, enabled.
    t[44..52].copy_from_slice(&[0, 8, 0, 0, 1, 0, 0, 0]);
    // I/O APIC: id 1, at FEC00000h, first interrupt 0.
    t[52..64].copy_from_slice(&[1, 12, 1, 0, 0x00, 0x00, 0xc0, 0xfe, 0, 0, 0, 0]);
    // Source overrides: bus 0 (ISA), source IRQ, global interrupt, flags.
    t[64..74].copy_from_slice(&[2, 10, 0, 0, 2, 0, 0, 0, 0x00, 0]); // IRQ0 -> 2, bus default
    t[74..84].copy_from_slice(&[2, 10, 0, 9, 9, 0, 0, 0, 0x0d, 0]); // SCI: level, active high
    t[84..94].copy_from_slice(&[2, 10, 0, 11, 11, 0, 0, 0, 0x0d, 0]); // PCI INTx on IRQ11
    // Local APIC NMI: processor 0, flags default, LINT1.
    t[94..100].copy_from_slice(&[4, 6, 0, 0, 0, 1]);
    t
}

// firmware_files(apic: bool), additions when `apic`:
//   let madt_at = rsdt + 44;                       // RSDT is 40 + 4 bytes
//   put32(&mut tables, rsdt + 40, madt_at as u32); // second RSDT entry
//   loader: one more add-pointer command for (TABLES, rsdt + 40),
//           one more checksum command for (TABLES, madt_at, 100, madt_at + 9),
//           and the RSDT checksum length becomes 44.
```

The override list must cover every interrupt number `_PRT` and the FADT's
SCI use. Today that is 9 and 11; IRQ14 is the legacy IDE line and stays
edge. If `_PRT` ever changes, this list changes in the same commit.

The I/O APIC model honours a redirection entry's trigger mode and mask but
not its polarity bit: "asserted" has one meaning inside the emulator. A
guest may program a hardwired `_PRT` line as active low or active high and
both must work. QEMU's and v86's models do the same
(`refs/qemu/hw/intc/ioapic.c` defines the bit and never applies it;
`refs/v86/src/rust/cpu/ioapic.rs` has no polarity).
- Checks: TinyCore with `--apic`: dmesg shows the local APIC timer and
  IO-APIC routing; NIC, IDE and USB interrupts work. Without `--apic`:
  serial output identical to before.
- EXITS: ► 4.3.

### Scene 4.3: Windows guests on the APIC

- ACTION:
  1. Windows 2000 frozen seed with `--apic`: boots as before on its PIC HAL
     (A6).
  2. Windows 7 frozen seed with `--apic`: note the HAL after the boot (A6),
     then M1, M2 and M4 on the RPi 5. M4 must report TPR writes per 1M
     steps and local APIC timer interrupts per second (A5).
- EXITS:
  - APIC within 5% of PIC ► leave act 4;
  - slower by more than 5% ► D4: if TPR writes are 3% or more of cycles ►
    I-TPR; otherwise Windows 7's preset stays on Retro. Leave act 4;
  - a guest stalls waiting for a timer the machine lacks ► 4.4;
  - Windows 2000 changes behaviour ► fix before leaving (contract 2).

### Scene 4.4: HPET (only on evidence from 4.3, 6.3 or 8.1)

- Enter with the guest, the symptom and the reference that says HPET is
  required. One timer block, legacy replacement routing, ACPI HPET table.
- EXITS: ► the scene that asked for it.

### D5: an APIC without a consumer (Lead, only on the way to L2b or L2c)

Keep `--apic` if scene 4.3 showed a guest that is faster or more correct
with it; otherwise remove the device, the MADT and the flag in one commit.

## ACT 6. Windows 10

### Scene 6.1: the seed

As scene 1.1, under QEMU `-cpu coreduo`, 2048 MiB, 32 GiB dynamic VHD,
BIOS boot on an MBR disk (A13; QEMU's default firmware, no OVMF).
Record edition and build; the USER picks them (an LTSC build has less
background load, a retail build is what most users have).

### Scene 6.2: first boot in r86

- ACTION: `r86-run --cpu-model modern --apic --mem-mib 2048
  --hdd tmp/modern/win10-seed.vhd --hdd-frozen --usb-tablet`.
- EXITS: desktop ► 6.4; table B symptom ► 6.3.

### Scene 6.3: blocker loop

Table B, budget 5. Expected first rows (A3): 5Dh, 5Ch, A5h.
- EXITS: fixed ► 6.2; budget spent ► G3 as "blocked".

### Scene 6.4: measure

M1-M4 as in 1.4, with the Mac run by the USER.
- EXITS: ► G3.

### G3: Windows 10 verdict (USER)

- FAIL or blocked ► **L2c** (through D5).
- MARGINAL ► D3, then 6.4 and G3 once more; a second MARGINAL is a FAIL.
- PASS ► act 7.

## ACT 7. Machine mode (Lead; the Intern does scene 7.1 only)

### Scene 7.1: installers inside r86

The seeds came from QEMU; users install from an ISO in r86.

- ACTION: install Windows 7 and Windows 10 from their ISOs in r86, with
  `--hdd-writable` on new disks, on the x86 host and on the Mac. Record wall
  time and any table B symptom.
- The slow part to expect: the boot manager loads the Windows PE RAM disk
  (hundreds of MiB) through SeaBIOS INT 13h, which is 16-bit code
  (interpreter only) doing PIO transfers (F27). M4 reports its share. If it
  dominates the install, the remedies in order are: make the interpreter's
  16-bit REP INSW path cheaper (helps Retro too); only then consider
  `CONFIG_ATA_DMA` in the ROM, which is a ROM change under contract 9.
- EXITS:
  - both install ► 7.2;
  - a symptom ► blocker loop, budget 3 ► 7.1;
  - an installer takes longer than the USER accepts ► 7.2 ships the preset
    with that limit stated, or not at all (USER).

### Scene 7.2: type, presets, documents

- One machine mode, chosen at VM creation and never changed afterwards
  (contract 1): Retro (Pentium II, PIC) and Modern (modern CPU, APIC).
  `--cpu-model` and `--apic` collapse into it; `vm_store` gains one field
  with the values `retro` and `modern`, and a missing field means Retro.
- Presets in `src/frontend/guest_os.rs`: "Windows 7" on the mode scenes 3.8
  and 4.3 measured faster; "Windows 10" on Modern. Hosts that cannot meet T
  do not offer them (Lead decides per frontend from the recorded numbers).
- Documents in the same commit: KNOWN_ISSUES "Limits by design" (the "No
  SSE" sentence, contract 5's limits), `design/CONFIGURATION.md`,
  `design/MACHINE_MODES.md` (Modern's status, options and code table),
  `design/IMPLEMENTATION_DEBT.md` (CPU-2 deleted; CPU-3 and FPU-3 updated),
  `TODO.md` "SSE then SSE2" removed, `tmp/perf/scoreboard.txt`.
- EXITS: ► **L3**, then G4 if the USER wants it.

## ACT 8. Snow Leopard, macOS hosts only

### G4: start? (USER)

Two decisions before any work, and one already made (the USER still
supplies its prop):

1. **Medium**: only Mac OS X 10.6 Server permits virtualization, on Apple
   hardware; the client licence forbids it. The UI names 10.6 Server.
2. **OSK**: the SMC must return Apple's OSK key or protected system
   binaries do not decrypt. r86 does not ship it. The USER decides where it
   comes from, or the act does not start.
3. **Loader** (decided: BIOS, contract 9): Mac OS X starts from a loader
   that SeaBIOS boots from disk or CD; the loader, not r86, gives the
   kernel the boot arguments and device tree it expects from EFI. r86
   writes no loader and ships none unless the USER decides its licence
   allows it; the USER names the loader and supplies it as a prop. If scene
   8.1 finds no such loader that boots the medium (A10), the act ends at
   L3.

- no, or decision 2 has no answer, or no loader is supplied ► **L3**.

### Scene 8.1: rehearsal in QEMU (replaces A10 with a device list)

- ACTION: boot the medium under QEMU TCG with SeaBIOS and the USER's
  loader (no OVMF, no EFI loader), `-cpu coreduo`, 32-bit kernel, and
  remove one thing at a time: HPET, PIIX IDE in place of AHCI, PS/2 in
  place of the USB keyboard, the SMC. Record what the kernel does each
  time. Run the census (M5).
- EXITS: D6 (Lead) writes the minimum device list.
  - the list needs AHCI, x86-64 or EFI firmware ► **L3**;
  - the census forecasts a FAIL by more than 2x ► **L3**;
  - otherwise ► 8.2.

### Scene 8.2: devices

- USB HID keyboard (and mouse if 8.1 requires) on the existing UHCI: a
  generic device, available on every host.
- SMC and the "Mac OS X 10.6" machine profile: compiled only with
  `cfg(target_os = "macos")`; the preset exists only in the Mac app. Linux,
  handheld, web, iOS and watchOS builds cannot run it.
- HPET through scene 4.4 if D6 lists it.
- EXITS: ► 8.3.

### Scene 8.3: first boot and blocker loop

Table B, budget 5, plus the kernel's own panics: unknown CPU model, missing
bus frequency from the loader, unknown MSRs (F9).
- EXITS: desktop ► 8.4; budget spent ► G5 as "blocked".

### Scene 8.4: measure

M1-M3 on the Mac only. T for Snow Leopard is set by the USER at G4.
- EXITS: ► G5.

### G5: verdict (USER)

- PASS ► **L4**.
- otherwise ► **L3**: the SMC and the profile are removed; the USB keyboard
  stays if it is correct and tested.

---

## 9. Constraints

- Test media and seeds stay local: no Windows or Apple media in Git or CI.
- These guests boot through tens of billions of instructions. No host but a
  Mac or a fast x86 machine is expected to meet T, and x86 hosts run the
  interpreter only. The Watch and A15 targets are out of scope for these
  guests and must lose nothing to them (gate P).
- A scene never widens its scope to make an exit true. Out of scope
  everywhere: x86-64, SMP, AHCI, UEFI (contract 9), native SSE lowering
  outside I-SSE, per-guest special cases.
- This file is the only plan for the Modern mode; the modes themselves are
  defined in `design/MACHINE_MODES.md`, which this plan updates in scene
  7.2. `tmp/refactor/plan-intern.txt` gets one line per scene when Lead
  schedules it.
