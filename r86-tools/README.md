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
`assets/README.md`). The Windows app workflow builds the public disc in CI.

- `disc/` is copied to the disc as laid out. Add a new public tool as a
  directory here.
- `r86-tool.c`, `r86-tool.h`: r86-tool (display modes, installer, absolute
  pointer helper), compiled to `R86TOOL/r86-tool.exe`.
  `R86TOOL/LICENSE.TXT` is the repository `LICENSE`.
- `vmdisp9x.sha256`: hashes of the original VMDisp9x files.

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
