#!/usr/bin/env python3
"""Build the public tools ISO and the internal full ISO in one run.

Requires pycdlib. See assets/README.md for preparing the local source files.
Missing sources fail the build instead of silently dropping an existing tool.
The public manifest contains redistributable tools. r86_tools_full.json adds
local VBEMP and Super PI assets; r86_tools_full.iso must never be published.
"""
import hashlib
import json
from pathlib import Path

import pycdlib


def build(assets, roots, image_name):
    directories = set()
    files = {'README.TXT': assets / (image_name + '.txt')}
    for name, source in roots.items():
        assert name and '/' not in name and name not in ('.', '..'), name
        source = assets / source
        if not source.is_dir():
            raise SystemExit(f'Missing tool directory: {source}')
        directories.add(name)
        for path in sorted(source.rglob('*')):
            destination = name + '/' + path.relative_to(source).as_posix()
            if path.is_dir():
                directories.add(destination)
            elif path.is_file():
                files[destination] = path

    iso = pycdlib.PyCdlib()
    iso.new(interchange_level=3, joliet=3, vol_ident=image_name.upper())
    for directory in sorted(directories):
        iso.add_directory(iso_path='/' + directory.upper().replace('-', '_'),
                          joliet_path='/' + directory)
    for destination, source in sorted(files.items()):
        # ISO 9660 excludes hyphens; Joliet retains names such as r86-tool.exe.
        iso.add_file(str(source), iso_path='/' + destination.upper().replace('-', '_') + ';1',
                     joliet_path='/' + destination)
    output = assets / (image_name + '.iso')
    temporary = output.with_suffix('.iso.part')
    iso.write(str(temporary))
    iso.close()
    temporary.replace(output)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    output.with_suffix('.sha256').write_text(f'{digest}  {output.name}\n')
    print(f'{output}: {len(files)} files, {output.stat().st_size} bytes, SHA256 {digest}')


def main():
    assets = Path(__file__).resolve().parents[1] / 'assets'
    public = json.loads((assets / 'r86_tools.json').read_text())
    internal = json.loads((assets / 'r86_tools_full.json').read_text())
    if public.keys() & internal.keys():
        raise SystemExit('Internal roots must not replace public tools')
    build(assets, public, 'r86_tools')
    build(assets, public | internal, 'r86_tools_full')


if __name__ == '__main__':
    main()
