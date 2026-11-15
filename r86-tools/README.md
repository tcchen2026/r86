# r86 tools disc

Everything for the public r86 tools CD (`r86_tools.iso`): guest utilities and
drivers for Windows. r86-run and the handheld insert it from beside the
executable (web console Tools → Install r86 Tools).

```sh
python3 -m pip install --target tmp/iso-tools pycdlib
PYTHONPATH=tmp/iso-tools python3 script/build_tools_iso.py
```

The build needs MinGW (`i686-w64-mingw32-gcc`) and writes
`assets/r86_tools.iso`; where the local `assets/r86_tools_full.json` exists,
also the internal `assets/r86_tools_full.iso` (never published; see
`assets/README.md`). The Windows app workflow builds the public disc in CI;
the Mac workflow bundles the release pinned by `assets/tools-disc.sha256`.

- `disc/` is copied to the disc as laid out. Add a new public tool as a
  directory here.
- `r86-tool.c`, `r86-tool.h`: r86-tool (display modes, installer, absolute
  pointer helper), compiled to `R86TOOL/r86-tool.exe`.
  `R86TOOL/LICENSE.TXT` is the repository `LICENSE`.
- `vmdisp9x.sha256`: hashes of the original VMDisp9x files.

The installer offers the host's recommended 16-bit resolution only on a
fresh install. A mode available now is applied and saved; a newly installed
driver or a mode added to VMDisp9x gets one Yes/No choice at the installing
user's next logon, through HKCU RunOnce. An unavailable mode is then skipped
silently. A driver-only install copies the utility for that choice without
starting the helper. Updates preserve the user's resolution and cancel an
older pending choice. See [the guest README](disc/R86TOOL/README.TXT).

Every successful Install/Update, including the command-line install and
driver display commands, asks "r86 tools installed. Restart now?" Yes
restarts Windows; No closes. Failures retain their error report. Host
recommendations use command 5 and capability bit 3 of
[the magic protocol](../src/machine/magic.rs); a Machine has none until its
frontend supplies one.

## VMDisp9x

MIT 2D display driver for Windows 95/98/Me, copyright (c) 2022 JHRobotics;
license text in `disc/VMDISP9X/license.txt`. `disc/VMDISP9X/` holds all 17
files of the official 2D release, unmodified (no SoftGPU/3D bundle):

- Repository: https://github.com/JHRobotics/vmdisp9x
- Release v1.2025.0.119 (published 2025-07-29; asset rebuilt 2025-08-03):
  https://github.com/JHRobotics/vmdisp9x/releases/download/v1.2025.0.119/vmdisp9x-1.2025.0.119b-driver-2d.zip
- Archive SHA256 e0d698a6089347a6a619ed439c092f247392fbafac6b342f4651c1380911fb23;
  version.txt 1.2025.0.119; INF DriverVer 08/03/2025,4.2025.0.119.

For r86's PCI\VEN_1234&DEV_1111, r86-tool installs `VMDISP9X\vmdisp9x.inf`'s
Qemu section (qemumini.drv/qemumini.vxd). One change is made while building
(`PATCHES` in `script/build_tools_iso.py`): qemumini.drv's FixModeInfo
minimum mode drops from 640x480 to 320x170 (bytes 0x2a31: 0280 to 0140,
0x2a39: 01e0 to 00aa). Disc copy SHA256
108f925d0a3642a2f8b45ec2023965bbcb04aecd51fe909c08965a89c49e2b7b.
