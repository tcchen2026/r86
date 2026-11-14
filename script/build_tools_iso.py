#!/usr/bin/env python3
"""Build the public tools ISO and the internal full ISO in one run.

Requires pycdlib and MinGW (i686-w64-mingw32-gcc). The public disc is
r86-tools/disc/ plus R86TOOL/r86-tool.exe, compiled from r86-tools/r86-tool.c,
and R86TOOL/LICENSE.TXT (the repository LICENSE); see r86-tools/README.md.
assets/r86_tools_full.json, local and not in Git, adds VBEMP and Super PI
directories for r86_tools_full.iso (see assets/README.md), which must never
be published; without it only the public image is built. Images are written
to assets/. A missing source fails the build instead of silently
dropping a tool. PATCHES changes disc copies only; the sources stay original.
"""
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tempfile

import pycdlib

ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / 'r86-tools'
ASSETS = ROOT / 'assets'

# Windows opens the installer window when the disc is inserted (Joliet names).
AUTORUN = b'[autorun]\r\nopen=R86TOOL\\R86-TOOL.EXE\r\nlabel=r86 tools\r\n'

# Plain C without the C runtime; runs on Windows 95 and NT 4 and later.
CC = ['i686-w64-mingw32-gcc', '-Os', '-s', '-nostdlib', '-ffreestanding',
      '-fno-asynchronous-unwind-tables', '-Wall', '-Wextra', '-e', '_start',
      '-Wl,--subsystem,windows:4.0', '-Wl,--major-os-version,4',
      '-Wl,--minor-os-version,0', '-Wl,--no-insert-timestamp']
LIBS = ['-lkernel32', '-luser32', '-lgdi32', '-ladvapi32']

# Disc path -> [(offset, original, replacement)]. VMDisp9x 1.2025.0.119's
# qemumini.drv refuses modes under 640x480 (modes.c FixModeInfo:
# cmp es:[bx],640 / cmp es:[bx+2],480); r86 lowers that to 320x170, half the
# handheld panel. The driver has no other minimum.
PATCHES = {
    'VMDISP9X/qemumini.drv': [
        (0x2a31, (640).to_bytes(2, 'little'), (320).to_bytes(2, 'little')),
        (0x2a39, (480).to_bytes(2, 'little'), (170).to_bytes(2, 'little')),
    ],
}


def tree(source, prefix=''):
    """Disc path -> file for every file under `source`."""
    if not source.is_dir():
        raise SystemExit(f'Missing tool directory: {source}')
    return {prefix + path.relative_to(source).as_posix(): path
            for path in sorted(source.rglob('*')) if path.is_file()}


def r86_tool():
    with tempfile.TemporaryDirectory() as scratch:
        exe = Path(scratch) / 'r86-tool.exe'
        subprocess.run(CC + ['-o', str(exe), str(TOOLS / 'r86-tool.c')] + LIBS, check=True)
        return exe.read_bytes()


def build(files, image_name):
    """Writes assets/IMAGE_NAME.iso; `files` maps disc paths to a Path or bytes."""
    for destination, patches in PATCHES.items():
        data = files[destination].read_bytes()
        for offset, original, replacement in patches:
            if data[offset:offset + len(original)] != original:
                raise SystemExit(f'{files[destination]}: unexpected bytes at {offset:#x}')
            data = data[:offset] + replacement + data[offset + len(original):]
        files[destination] = data
    directories = {'/'.join(path.split('/')[:i])
                   for path in files for i in range(1, path.count('/') + 1)}

    iso = pycdlib.PyCdlib()
    iso.new(interchange_level=3, joliet=3, vol_ident=image_name.upper())
    for directory in sorted(directories):
        iso.add_directory(iso_path='/' + directory.upper().replace('-', '_'),
                          joliet_path='/' + directory)
    iso.add_fp(io.BytesIO(AUTORUN), len(AUTORUN), iso_path='/AUTORUN.INF;1',
               joliet_path='/autorun.inf')
    for destination, source in sorted(files.items()):
        data = source if isinstance(source, bytes) else source.read_bytes()
        # ISO 9660 excludes hyphens; Joliet retains names such as r86-tool.exe.
        iso.add_fp(io.BytesIO(data), len(data), iso_path='/' + destination.upper().replace('-', '_') + ';1',
                   joliet_path='/' + destination)
    output = ASSETS / (image_name + '.iso')
    temporary = output.with_suffix('.iso.part')
    iso.write(str(temporary))
    iso.close()
    temporary.replace(output)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    output.with_suffix('.sha256').write_text(f'{digest}  {output.name}\n')
    print(f'{output}: {len(files) + 1} files, {output.stat().st_size} bytes, SHA256 {digest}')


def main():
    public = tree(TOOLS / 'disc')
    public['R86TOOL/r86-tool.exe'] = r86_tool()
    public['R86TOOL/LICENSE.TXT'] = ROOT / 'LICENSE'
    manifest = ASSETS / 'r86_tools_full.json'
    full = dict(public, **{'README.TXT': ASSETS / 'r86_tools_full.txt'})
    internal = json.loads(manifest.read_text()) if manifest.exists() else {}
    for name, source in internal.items():
        assert name and '/' not in name and name not in ('.', '..'), name
        if any(path.split('/')[0] == name for path in public):
            raise SystemExit(f'Internal root {name} must not replace a public tool')
        full.update(tree(ASSETS / source, name + '/'))
    build(public, 'r86_tools')
    if manifest.exists():
        build(full, 'r86_tools_full')


if __name__ == '__main__':
    main()
