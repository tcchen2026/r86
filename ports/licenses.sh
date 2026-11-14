#!/bin/sh
# Installs the licenses and source notes of what r86 packages ship (r86,
# ymfm, QEMU USB, fatfs, SeaBIOS) into DIR. Used by ports/share.sh,
# script/build_windows.sh and script/build_macos.sh; POSIX cp, no GNU install.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
licenses=$1
mkdir -p "$licenses/r86" "$licenses/ymfm" "$licenses/qemu-usb" "$licenses/fatfs" "$licenses/seabios"
cp "$root/LICENSE" "$licenses/r86/LICENSE"
cp "$root/src/devices/audio/ymfm-license.txt" "$licenses/ymfm/LICENSE"
cp "$root/src/devices/usb/qemu-license.txt" "$licenses/qemu-usb/LICENSE"
cp "$root/vendor/fatfs/LICENSE.txt" "$root/vendor/fatfs/provenance.txt" "$licenses/fatfs/"
cp "$root/vendor/seabios/COPYING" "$root/vendor/seabios/COPYING.LESSER" \
    "$root/firmware/seabios/bios.config" "$root/firmware/seabios/vga.config" \
    "$licenses/seabios/"
cat >"$licenses/seabios/SOURCE" <<'TXT'
bios.bin and vgabios-stdvga.bin are SeaBIOS 1.17.0 (LGPL-3.0), upstream
commit b52ca86e094d19b58e2304417787e96b940e39c6 (https://www.seabios.org/),
with r86's changes, built with the configs in this directory by r86's
script/build_seabios.py. The corresponding source is vendor/seabios in r86's
repository (https://github.com/tcchen2026/r86); vendor/README.md lists the
changes.
TXT
