Think carefully then answer. Think like Bellard.

# Intern and Lead

One Lead agent and one Intern agent.

1. Always use "Intern", "Lead" in handoff/other documents.
2. Lead sends instructions and review.
3. Only Intern can touch ssh rpi5, Lead never touches it, Lead always delegate to Intern on rpi5-related tasks.
4. The Intern works in the main tree, /work/r86, on `main`: no Git
   worktrees, clones or branches. Do not change Git state (commit, checkout,
   switch, reset, restore, stash, clean, rebase, `apply --index`); read-only
   Git is fine. Hand off `git diff -- <your files>` and the file list; Lead
   commits. The plan is tmp/refactor/plan-intern.txt.
5. Build with `CARGO_TARGET_DIR=tmp/target-intern`. Build a timed baseline
   from a `git archive <commit>` export under tmp/build/; never edit an
   export, and delete it after the build.
6. Boot test and benchmark disks with `r86-run --hdd-frozen` on a shared
   read-only seed (Windows 2000: tmp/usb/win2000-clean.vhd). Never copy or
   write a seed for a run. A new seed is written once with `--hdd-writable`,
   its SHA256 recorded, then used frozen.
7. Reproduce a bug seen on an existing disk, including a user's installed
   VM, by booting that disk with `--hdd-frozen`: r86 opens it read-only
   and guest writes are discarded at exit. Do not install a new
   disk to reach the same state. Boot it only while no other r86 process
   has it open writable. Never write a user's disk or send input to a
   user's running VM unless the user asks.

For lead: just steer, don't queue, review its progress and feel free to steer messages.

# Project working rules

## Scope and documentation

Write first-party documentation, code comments and diagnostic reports in concise
English for AI readers. State current contracts, code paths, commands and limits.
Keep each fact in one authoritative place; link instead of duplicating plans.
Preserve Unicode test fixtures, UI localization and upstream/license text.

The project is being redesigned around a fast interpreter suitable for iOS and
watchOS App Store distribution without JIT or virtualization privileges. HardMMU (KVM
host, EL1 monitor) was removed after measuring slower; see design/REWRITE.md. Remove poor
abstractions, redundant checks, speculative machinery, and unnecessary fallback
layers. Read [design/REWRITE.md](design/REWRITE.md)
for the agreed direction and [design/VALIDATION.md](design/VALIDATION.md) for
build and verification commands.

For macOS app work, start with the development handoff,
`private_docs/GUIDE_FOR_FUTURE_MAC_DEV.md` (local, not in Git). It records the
current code, missing platform work and measurement limits; it does not claim that the Mac port or its proposed UI has already been validated.

Completed experiments, superseded documents, consultations and rejected patches
stay out of the repository; keep them in local backups. They are historical
evidence, not active instructions. Do not carry their API shapes, milestone order, or implementation
mandates into the rewrite without a current reason. The code describes the
existing implementation, not the required replacement structure.

## Machine modes: Retro and Modern

- **Retro** is the machine implemented today (Pentium II class CPU, 8259 PIC,
  ACPI) and the mode of every existing VM. **Modern** is planned and not
  implemented (SSE/SSE2/SSE3, NX, APIC; Windows 7 and 10 32-bit).
- A mode is a CPU model plus the APIC platform inside the one shared
  interpreter, compiled once. Do not fork or duplicate the interpreter,
  devices or JITs per mode. Modern must not slow Retro.
- Modern stays 32-bit and boots through SeaBIOS, like Retro (user,
  2026-11-14).
- Say "Retro" and "Modern" (values `retro`, `modern`).
- Definitions and rules: [design/MACHINE_MODES.md](design/MACHINE_MODES.md).
  Modern's plan: `modern/PLAN_MODERN.md` (local, not in Git).

## r86chip: ISA authority and change order

r86chip is a RV32-with-custom-extension cpu, for accelerated x86 emulation.

Read r86chip/README.md if you are working on it.

## JIT focus: 32-bit Windows workloads

- Both the A64 JIT and r86chip's translator/extensions prioritize the 32-bit
  protected-mode hot paths used by Windows 98, Windows 2000 and Windows XP.
  This includes kernel and user execution, normal paging, and the code those
  operating systems and their applications actually run. Judge additions by
  correctness and measured benefit on these workloads, not by native ISA coverage.
- Rule: 16-bit code is interpreter-only. No code generator (A64, ARMv6K,
  Wasm, r86chip) compiles real mode, virtual-8086 mode or a 16-bit code
  segment (CS default size 16); make those paths faster in the shared
  interpreter instead. The test is the code segment's default size, not the
  operand size: 32-bit code using 16-bit operands or 66h/67h prefixes still
  compiles. Reopen only with a measurement showing 16-bit code dominating a
  priority workload (for example Windows 98's 16-bit USER/GDI paths).
- BIOS/setup paths, mode transitions, cold code and uncommon/complex
  instructions may also stay in the shared interpreter. Native coverage for
  every x86 mode is not a requirement for either JIT or the FPGA extensions.
  Keep the interpreter complete for the chosen guest CPU model so boot and
  mixed-mode software continue to work.
- Guest execution mode and operand size are different. A 32-bit code segment
  still uses byte/word operands, size overrides and segment addressing. Do not
  assume flat segments, ring 3 only, or disabled paging merely because the
  acceleration target is 32-bit. Native paths must preserve the applicable x86
  semantics; unsupported cases return to the interpreter at a precise boundary.
- Entry guards and interpreter handoffs must preserve guest state, fault
  ordering, interrupt/shadow behavior and time accounting. An ordinary coverage
  miss uses shared interpretation; an emulator bug is reported, not concealed
  by retrying through another engine. Do not add a second semantic implementation
  just to accelerate a rare mode.

## Implementation discipline

- Use one Cargo package, `r86`. Select host, frontend and bare-metal
  builds with features, targets, and binary entry points. Organize source into
  modules; do not recreate the former workspace as packages or generic layers.
- Prefer a single thread for the emulator core, including CPU execution, device
  I/O, interrupts and event scheduling. Keep the design usable through no_std
  bare-metal execution without requiring OS threads or an async runtime.
  WebSocket connection handling may use separate threads; machine state remains
  owned by the core execution thread.
- Keep synchronous IDE file reads, writes and flushes. Their presence alone is
  not technical debt and does not justify adding storage worker threads.
- Let tightly related emulator and frontend code cooperate directly. Future GPU
  forwarding may share the frontend's GPU resources; do not impose a synthetic
  transport or a separate device framework just to isolate those modules.
- Build the Rust i386 interpreter as the primary production path, also usable
  as the semantic reference. It must work without KVM, Hypervisor.framework,
  JIT, or executable-memory tricks. Share x86 semantics across any backends.
- Optimize the shared interpreter so all supported hosts, including r86chip,
  can benefit. Do not fork the interpreter by host ISA. Prefer reducing common
  work; keep necessary native primitives small and selected at build time.
  Check performance changes on available x86_64 and ARM64 hosts, and distinguish
  preserved compatibility from speedups not yet measured on other hardware.
- Prioritize interpreter performance on Apple Watch Series 9 (S9) and Apple A15
  devices. Measure them separately, including sustained guest throughput on the
  Watch. RPi/x86 measurements support iteration and compatibility checks; they
  do not establish Apple performance. Keep one shared interpreter and retain
  guest semantics on every host. See [design/APPLE.md](design/APPLE.md) for the
  target and measurement scope.
- Optional backends must earn their complexity through measurements on the
  target platform; Linux/RPi results do not establish an Apple benefit.
  Optional backends must not burden the ordinary interpreter path.
- Do not overdesign. Choose simple ownership and direct interfaces; add machinery
  only for a concrete current need. Delete obsolete production paths when
  replacing them; avoid speculative frameworks and unused modes.
- Validate external input at its boundary. Remove repeated checks of established
  internal invariants. Keep checks needed for guest semantics or memory safety.
- Handle expected guest exceptions as guest behavior. Report emulator bugs
  clearly; do not hide them with catch-all recovery, retries, or silent fallback.
- Keep guest time deterministic. Retain no_std compatibility for code that runs
  bare-metal; select platform facilities at build time.
- Keep unsafe and assembly boundaries small and explicit. Any recoverable native
  fault path must be valid for the compiler and hardware; host/native data must
  have an explicit representation rather than depend on Rust object layout.
- Preserve architectural effects while simplifying: segmentation, paging,
  privilege, EFLAGS, fault ordering, A/D bits, restartable writes, and MMIO.
  Tests should assert observable behavior rather than obsolete implementation
  structure. SoftMMU is a comparison path, not proof that its own code is bug-free.
- Require evidence for hot-path complexity. For performance work, inspect the
  relevant tiny386 source and both linked objdumps; use matched end-to-end
  results to judge changes. Keep diagnostics off the normal path where practical.

## References

`refs/` contains external study inputs, not production emulator code. Do not
copy implementation logic or mechanically translate reference code into Rust.

- Always consult `refs/tiny386` first for I/O, interrupts, CPU behavior, memory
  semantics, device compatibility and machine integration. Use its simple design
  and execution cost as the starting point for our implementation.
- Prioritize practical OS behavior and removing technical debt.
  Do not turn compatibility fixes into cycle-exact hardware simulation or
  add precision machinery without a demonstrated guest requirement.
- Use `refs/v86` for additional OS compatibility evidence. Consult 86Box, Bochs
  and datasheets when missing behavior, ambiguity or a concrete disagreement
  needs resolving. An emulator's shortcuts are not hardware requirements.
- For segmentation, paging, fault priority, CRx and A/D, use Bochs for selected
  oracle comparisons within the chosen x86 model's defined behavior. For PC
  devices, use 86Box and Bochs where further verification is needed.

Record only references actually consulted. Changes under `refs/` require an
update to `private_docs/REBUILD_REFS.md` (local, not in Git) in the same batch.
Existing local reference modifications are not permission to overwrite them.

## Validation and changes

Do not over-test. Before running any test or benchmark, judge its necessity from
both simulated Bellard and Linus review perspectives:

- Bellard: does this resolve a concrete semantic uncertainty or inform a real
  performance decision, with the smallest useful experiment?
- Linus: does this check observable behavior or a credible regression risk,
  rather than repeat implementation details or produce reassuring numbers?

Run it only when both perspectives consider it necessary. Make this judgment
internally; do not turn it into a review ceremony. Prefer existing focused
checks. Do not repeat passing checks without a relevant change or new evidence,
and do not run full suites or boot benchmarks merely because they are listed.
Complete a coherent implementation batch, including related optimizations and
code review, before testing it. Do not interrupt unfinished work to benchmark
each small edit; prioritize a useful complete result over incremental test runs.
Use one useful pilot to reject an optimization; reserve the agreed two measured
runs for confirming a promising result. No routine warm-ups or extra repetitions.

Before an implementation batch, identify its scope, consulted references, and
relevant checks. Use [design/VALIDATION.md](design/VALIDATION.md) to select tests
and boot gates appropriate to the change, subject to the necessity rule above;
the guide is not an automatic checklist. Documentation-only work needs document
and diff checks, not tests or emulator boot runs.

Keep commits coherent and inspect the diff for unrelated edits and generated
artifacts. Do not replace working behavior with untested design claims. Existing
SoftMMU performance and code-size acceptance criteria remain in the validation
guide; old archived thresholds do not apply.

`script/ws_cli.py` is the WebSocket protocol reference. Keep the singular
`script/` path and update its module docstring when the protocol changes.

Keep guest utilities and drivers together on the r86 tools disc. Its public
sources are tracked in `r86-tools/` (see its README): add future public tools
under `r86-tools/disc/` and rebuild with `script/build_tools_iso.py`, which
writes `assets/r86_tools.iso`; retain existing tools instead of creating a
separate ISO for each addition. Preserve original executables, documentation
and licenses, with provenance and checksums recorded. Generated images and
the full disc's private extras stay outside Git.

## Local artifacts

Git tracks the SeaBIOS ROMs (`firmware/seabios/`) but not the DOS kit, the
tools ISO or disks; for those, the current tree's build script decides which
copy is newest. Published pins such as `assets/freedos-kit.sha256` record the
last uploaded release asset and lag the scripts between releases: a local build
that differs from a pin is usually the newer, unpublished candidate.

- Local runs, the handheld simulator and device packages built here use the
  ROMs in Git and the newest kit and tools ISO built from the current tree.
  Publishing a kit or changing its pin needs the user's OK.
- Call a generated artifact outdated only with evidence: it differs from the
  ROMs in Git or from a fresh build of the current tree
  (`python3 script/build_freedos_image.py --share-dir tmp/<scratch>` rebuilds
  the kit byte-identical). A mismatch with a pin is not that evidence.
- A cleanup proposal lists each file with that evidence and names every disk or
  seed by path. Approval of a group such as "old kits" covers only the files
  listed and checked.
