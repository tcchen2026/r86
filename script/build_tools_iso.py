#!/usr/bin/env python3
"""Rebuild assets/r86_tools.iso from its cumulative directory manifest.

Requires pycdlib. See assets/README.md for preparing the local source files.
Missing sources fail the build instead of silently dropping an existing tool.
"""
import hashlib
import json
from pathlib import Path

import pycdlib


def main():
    assets = Path(__file__).resolve().parents[1] / 'assets'
    roots = json.loads((assets / 'r86_tools.json').read_text())
    directories = set()
    files = {'README.TXT': assets / 'r86_tools.txt'}
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
    iso.new(interchange_level=3, joliet=3, vol_ident='R86_TOOLS')
    for directory in sorted(directories):
        iso.add_directory(iso_path='/' + directory.upper(),
                          joliet_path='/' + directory)
    for destination, source in sorted(files.items()):
        iso.add_file(str(source), iso_path='/' + destination.upper() + ';1',
                     joliet_path='/' + destination)
    output = assets / 'r86_tools.iso'
    temporary = output.with_suffix('.iso.part')
    iso.write(str(temporary))
    iso.close()
    temporary.replace(output)
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    (assets / 'r86_tools.sha256').write_text(f'{digest}  {output.name}\n')
    print(f'{output}: {len(files)} files, {output.stat().st_size} bytes, SHA256 {digest}')


if __name__ == '__main__':
    main()
