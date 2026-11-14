#!/bin/sh
# Installs the licenses and source notes of what r86 packages ship (r86,
# ymfm, QEMU USB, fatfs, SeaBIOS) into DIR. Used by ports/share.sh and
# script/build_windows.sh.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
licenses=$1
install -D -m 0644 "$root/LICENSE" "$licenses/r86/LICENSE"
install -D -m 0644 "$root/src/devices/audio/ymfm-license.txt" "$licenses/ymfm/LICENSE"
install -D -m 0644 "$root/src/devices/usb/qemu-license.txt" "$licenses/qemu-usb/LICENSE"
install -D -m 0644 "$root/vendor/fatfs/LICENSE.txt" "$root/vendor/fatfs/provenance.txt" \
    -t "$licenses/fatfs"
install -D -m 0644 "$root/vendor/seabios/COPYING" "$root/vendor/seabios/COPYING.LESSER" \
    "$root/firmware/seabios/bios.config" "$root/firmware/seabios/vga.config" \
    "$root/firmware/seabios/acpi-features.patch" "$root/firmware/seabios/handheld-modes.patch" \
    -t "$licenses/seabios"
cat >"$licenses/seabios/SOURCE" <<'TXT'
bios.bin and the vgabios-*.bin ROMs are SeaBIOS 1.17.0 (LGPL-3.0), upstream
commit b52ca86e094d19b58e2304417787e96b940e39c6
(https://www.seabios.org/), built with the configs and patches in this
directory by r86's script/build_seabios.py.
TXT
