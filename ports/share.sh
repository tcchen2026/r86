#!/bin/sh
# Installs what r86-handheld loads from beside its executable into DIR:
# SeaBIOS, the DOS kit from script/build_freedos_image.py (built into
# tmp/freedos/share) and the licenses with their source notes.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
app=$1
install -D -m 0644 "$root/firmware/seabios/bios.bin" "$app/bios.bin"
install -m 0644 "$root/firmware/seabios/vgabios-handheld.bin" "$app/"
mkdir -p "$app/dos"
# Keep guest files, boot templates, corresponding sources and provenance together.
cp -R "$root/tmp/freedos/share/dos/." "$app/dos/"

licenses=$app/licenses
install -D -m 0644 "$root/LICENSE" "$licenses/r86/LICENSE"
install -D -m 0644 "$root/vendor/fatfs/LICENSE.txt" "$root/vendor/fatfs/provenance.txt" \
    -t "$licenses/fatfs"
install -D -m 0644 "$root/vendor/seabios/COPYING" "$root/vendor/seabios/COPYING.LESSER" \
    "$root/firmware/seabios/bios.config" "$root/firmware/seabios/vga.config" \
    "$root/firmware/seabios/acpi-features.patch" "$root/firmware/seabios/handheld-modes.patch" \
    -t "$licenses/seabios"
cat >"$licenses/seabios/SOURCE" <<'TXT'
bios.bin and vgabios-handheld.bin are SeaBIOS 1.17.0 (LGPL-3.0), upstream
commit b52ca86e094d19b58e2304417787e96b940e39c6
(https://www.seabios.org/), built with the configs and patches in this
directory by r86's script/build_seabios.py.
TXT
install -D -m 0644 "$root/assets/freedos.json" "$licenses/freedos/freedos.json"
