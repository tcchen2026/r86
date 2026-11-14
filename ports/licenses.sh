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
    "$root/firmware/seabios/acpi-features.patch" "$root/firmware/seabios/handheld-modes.patch" \
    "$licenses/seabios/"
cat >"$licenses/seabios/SOURCE" <<'TXT'
bios.bin and the vgabios-*.bin ROMs are SeaBIOS 1.17.0 (LGPL-3.0), upstream
commit b52ca86e094d19b58e2304417787e96b940e39c6
(https://www.seabios.org/), built with the configs and patches in this
directory by r86's script/build_seabios.py.
TXT
