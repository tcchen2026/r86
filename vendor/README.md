# Vendored dependencies

`seabios/` is a source export of
[SeaBIOS rel-1.17.0](https://github.com/coreboot/seabios/tree/rel-1.17.0),
commit `b52ca86e094d19b58e2304417787e96b940e39c6`, upstream tree
`00ca2288a3318f220d235c2e2cb9ca4f6f430874`, edited in place with r86's
changes. These are all of them; keep this list current:

- `src/fw/paravirt.c`: the QEMU path calls `find_acpi_features()`, so the PM
  timer and APM shutdown port come from r86's FADT instead of a recognized
  PCI PM chipset.
- `src/bootsplash.c` `r86_splash()`, called from `src/boot.c`
  `interactive_bootmenu()`, declared in `src/util.h`: during the boot menu
  wait, VGA mode 13h shows the logo from the `etc/r86-splash` fw_cfg file,
  "Press ESC for boot menu" and a bar stepping 10-30-70-100%, then text mode.
  Without the file the wait is unchanged. r86 supplies it through
  `Machine::enable_boot_splash`.
- `src/bootsplash.c`: the label in `etc/r86-engine` appears at the top right
  after the POST banner, during the r86 splash (40 columns), and on return
  to text mode (80 columns). No file means no label; the cursor is preserved.
- `vgasrc/svgamodes.c`: VBE mode 1A0h, 640x340x16, twice the M5Stack
  CardputerZero's 320x170 LCD; modes 1A1h/1A2h, 936x666 at 16/32 bpp,
  half the iPhone Duo's 1878x1335 upper half in laptop posture, with width
  rounded down to a multiple of eight pixels.

The upstream repository is <https://github.com/coreboot/seabios.git>.
The export includes all tracked files, including the original copyright
notices, [COPYING](seabios/COPYING), and [COPYING.LESSER](seabios/COPYING.LESSER).
SeaBIOS identifies its distribution license as LGPLv3; individual build tools
and other files retain their upstream license notices.

This is guest firmware source. Its build configurations and ROM artifacts are
described in [firmware/seabios](../firmware/seabios/README.md).

To reproduce the upstream export into a new directory and see r86's changes:

```sh
git clone --branch rel-1.17.0 --depth 1 https://github.com/coreboot/seabios.git /tmp/seabios-upstream
git -C /tmp/seabios-upstream rev-parse HEAD
mkdir /tmp/seabios-export
git -C /tmp/seabios-upstream archive b52ca86e094d19b58e2304417787e96b940e39c6 | tar -x -C /tmp/seabios-export
diff -r /tmp/seabios-export vendor/seabios
```
