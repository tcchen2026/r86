# r86

**A heavily optimized x86 emulator for ARM32 and ARM64 devices.**

r86 emulates a complete PC and boots DOS, Linux and even Windows. Our main focus
is iOS, macOS and Raspberry Pi-based devices such as the M5Stack Cardputer Zero.
The core interpreter needs no JIT, hypervisor or executable memory; optional
JITs add speed on ARM64 and ARMv6K.

## Try it now

Get `TinyCore-current.iso` from [tinycorelinux.net](http://tinycorelinux.net/downloads.html)
and the SDL2 development package (`libsdl2-dev`), then:

```sh
cargo run --release --features sdl2-frontend --bin r86-run -- \
  --sdl2 --iso TinyCore-current.iso \
  --bios firmware/seabios/bios.bin --vga-bios firmware/seabios/vgabios-stdvga.bin
```

## Why it's fast

![ReactOS 0.4.15 LiveCD boot time: r86 vs QEMU on a Raspberry Pi 5](media/reactos-boot-time.svg)

- **No middle layer.** QEMU translates x86 into its own intermediate code (TCG)
  before reaching ARM, and its interpreter runs that code, not x86. r86 goes
  straight from x86 to ARM, and its JIT keeps EAX to EDI in ARM registers
  across blocks.
- **Made for small chips.** The interpreter core has a hard 256 KiB code
  budget, so it stays in the CPU caches of phones, watches and the Pi.
- **No busywork.** Most x86 instructions update six EFLAGS bits that nobody
  reads. r86 keeps the operands and computes a flag only when it is needed.
  Devices are checked between batches of thousands of instructions, not after
  every one.
- **One engine.** Whatever the JIT does not handle runs in the same
  interpreter, with identical registers and flags, so speed never costs
  compatibility.

## License

r86 is licensed under the [Apache License 2.0](LICENSE). The bundled SeaBIOS
([source](vendor/seabios), [firmware](firmware/seabios)) is LGPLv3, and
[the tiny386 patch](patches/tiny386-reactos-compat.patch) is BSD-3-Clause.

## Credits

**Projects r86 builds on.** r86's design started from
[tiny386](https://github.com/hchunhui/tiny386) (BSD-3-Clause),
which remains its performance baseline. [v86](https://github.com/copy/v86)
(BSD-2-Clause) is used as a reference for operating-system compatibility.
[SeaBIOS](https://www.seabios.org/) (LGPLv3) is bundled as the PC BIOS and VGA
BIOS; its source is in [vendor/seabios](vendor/seabios). We thank their
authors.

**How x86 behavior was checked.** r86 follows Intel's published architecture
manuals. Where details are ambiguous or undocumented, such as fault ordering,
paging and device edge cases, [QEMU](https://www.qemu.org/),
[Bochs](https://bochs.sourceforge.io/) and [86Box](https://86box.net/) were
consulted as references: their source code was read to understand their
behavior, and Bochs and QEMU are run in testing, as installed programs, to
compare results. r86 does not include code from these projects.

**How the code is reviewed.** r86 is written with the help of AI coding
assistants. Such tools are trained on large amounts of public code, which may
include other emulators, and can occasionally produce code that resembles their
training material. To reduce that risk, r86's code is reviewed against the
open-source projects it could most plausibly resemble. Code that matches or
closely resembles work under terms r86 does not accept is removed and rewritten
independently. If you believe something in r86 comes from your work without
credit, please open an issue: we take such reports seriously and review each
one.
