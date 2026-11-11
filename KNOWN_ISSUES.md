# Known issues

All known defects and costs at this commit, by priority. The goal is speed
for DOS games and Windows guests. One entry each: effect, trigger, code.
Delete an entry in the commit that fixes it. `[?]` = reported by an audit
or review and not yet re-checked against this commit.

Tags: perf, cpu, device, host (frontends, console, host services), tool, build.

## P0: a guest or the network can hang or kill the emulator

- host: one WebSocket text frame with a `\u` escape followed by a
  multi-byte character panics `parse_json_string`
  (src/frontend/protocol.rs); release builds abort on panic.

## P1: large speed cost, or ordinary guests hit it

- perf: the interpreter ends its batch after every far CALL/JMP, IRET,
  IN/OUT, INS/OUTS, STI and SS load. STI and its one-instruction shadow
  cause 98.9% of these returns (36M per 400M instructions of a real-mode
  DOS game boot); each return costs about 27 ns of batch exit and re-entry
  on the RPi 5 interpreter, about 12% of its CPU (`run_batch_inner`).
- device: the FreeDOS kit's JEMMEX profile (S=DA00-EFFF) gets 3 KiB of
  upper memory instead of 59 KiB: the bundled SeaBIOS write-protects
  DA000-E7FFF through PAM, which r86 now enforces
  (firmware/seabios/bios.config lacks `CONFIG_WRITABLE_UPPERMEMORY=y`).
- perf: self-modifying code under the A64 JIT recompiles on every change;
  Doom costs 28% more cycles with the JIT than with the interpreter.
- perf: MMX in JIT code calls a shared helper per instruction (38% of
  Windows 2000 Start-menu fade cycles); no native lowering.
- perf: on Windows window switches the interpreter runs 0.34% of guest
  instructions for 11.5% of cycles (INT/IRET, short budgets, page-split
  instructions); JIT dispatch takes another 21%.
- cpu: x87 registers are `f64`: no 80-bit precision, precision and
  rounding control ignored, tag word and deferred exceptions not modelled,
  transcendental functions approximated. [?]
- cpu: TF single-step traps and DR0-DR3 breakpoints are not delivered. [?]
- host: the console WebSocket accepts any Origin; a web page open in a
  LAN browser can drive the VM (`WebSocket::accept`, src/frontend/ws.rs).

## P2: specific guests or drivers; smaller or unmeasured cost; bad input

- perf: shipped builds store the instruction address on every instruction
  (`host` enables `bus-instruction-context`); cost not measured.
- perf: every port write outside USB, sound and the NIC raises attention,
  making the next batch one instruction long.
- perf: 16-bit, real-mode and virtual-8086 code is never compiled.
- cpu: an instruction longer than 15 bytes with fewer than 15 prefixes
  executes instead of raising #GP(0).
- cpu: when a batch's budget ends exactly where an STI or SS-load shadow
  expires, the batch returns without polling, so a pending IRQ waits for
  the next attention-causing instruction or device event
  (`run_batch_inner` attention_at_limit, src/x86/interp.rs).
- cpu: INT3 and INTO in virtual-8086 mode with IOPL < 3 raise #GP. [?]
- cpu: byte and word read-modify-write instructions report a read page
  fault where a write fault is due; zero-count shifts skip the write
  check. [?]
- cpu: execute page faults set the I/D error-code bit although the model
  has neither NX nor SMEP. [?]
- device: VGA text assumes 80 columns and 400 lines; blink, cursor shape,
  9th dot and font select are not rendered; 16-colour modes ignore row
  repeat (200-line EGA modes); CGA modes 4-6 have no renderer; no pel
  panning or line compare; retrace is fixed at 60 Hz. [?]
- device: IDE drive selection clears the other drive's pending
  interrupt; ATAPI lacks READ SUB-CHANNEL and reports medium type 0 with
  an empty drive. [?]
- device: the ACPI tables declare an MPU-401 at 330h that does not exist.
- host: `files-get` reads a whole guest file into memory; an interrupted
  `files-put` overwrite loses the original; a disc built from uploaded
  files is staged in RAM with no entry limit. [?]
- host: a host socket error in the user-mode network, an audio queue
  error, or a settings write error ends the running session. [?]
- host: the console accept loop stops at the first accept error; dead
  clients keep their slot and their held keys; JSON keys are found by
  substring search. [?]
- host: a VHD that fails to grow (disk full) fails every later read; the
  handheld does not re-check a VM's RAM size against the device. [?]
- host: the Mac app is built with abort-on-panic, so one VM's panic ends
  the app. [?]
- build: no CI builds or tests the emulator (only the Mac app); the
  no-JIT translation-cache layout used by the Apple build is not in the
  default test run.
- build: README says r86 includes no QEMU code; the USB controller is
  adapted from QEMU's MIT files, and ymfm (OPL2) is not credited there.

## P3: rare paths, model honesty, structure

- cpu: CR4 reserved bits are masked silently; RDPMC returns 0; P6 MSRs
  are storage only; LES/BOUND second fields ignore 16-bit address wrap;
  split loads read the first page before the second page can fault. [?]
- host: unclaimed-access diagnostics grow without bound; a triple fault
  stores a formatted dump of the whole CPU. [?]
- tool: r86-tool formats into a 256-byte buffer and sets display modes
  without CDS_TEST. [?]
- structure: the `Bus` trait has 70 methods; devices live in four places
  and port dispatch in six functions; the translation cache has two
  layouts selected by the `jit` feature.

## Deferred: no known guest depends on them

- device: a page-table accessed/dirty store into device memory or ROM is
  refused by `HostBus::check_write_range` and stops the machine.
- device: reading the IDE data port during a write transfer drops the
  write (`IdeChannel::read_data_u8`, `finish_data_if_drained`).
- cpu: FXSAVE/FXRSTOR test alignment on the segment offset, not the
  linear address (`aligned_fxsave_memory`).

## Limits by design

No SSE. Sound is SB16 with OPL2: no OPL3, MPU-401, recording or ADPCM.
Networking is IPv4 TCP, ARP, DHCP and DNS A only. USB is UHCI with one
HID tablet.

## Unexplained

- VSHARE.386 without DOS SHARE loops in FreeDOS on a cyclic buffer chain.

## Not measured

Sound (OPL2, SB16) CPU share in games; any workload on the CardputerZero
or an Apple device; Windows 98 and XP guests.
