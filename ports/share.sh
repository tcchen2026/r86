#!/bin/sh
# Installs what r86-handheld loads from beside its executable into DIR:
# SeaBIOS, the DOS kit from script/build_freedos_image.py (built into
# tmp/freedos/share) and the licenses with their source notes.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
app=$1
install -D -m 0644 "$root/firmware/seabios/bios.bin" "$app/bios.bin"
install -m 0644 "$root/firmware/seabios/vgabios-handheld.bin" "$app/"
# One archive keeps guest files, boot templates, corresponding sources and
# provenance together.
install -m 0644 "$root/tmp/freedos/share/dos.zip" "$app/"
# The public guest tools disc (script/build_tools_iso.py; not in Git) carries
# its own licenses. Without it the VM menu has no "Install r86 tools".
if [ -f "$root/assets/r86_tools.iso" ]; then
    install -m 0644 "$root/assets/r86_tools.iso" "$app/"
fi

"$root/ports/licenses.sh" "$app/licenses"
install -D -m 0644 "$root/assets/freedos.json" "$app/licenses/freedos/freedos.json"
