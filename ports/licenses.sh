#!/bin/sh
# Installs the licence material every r86 package ships into DIR: r86's
# licence, THIRD_PARTY_NOTICES.md (adapted code, vendored source, SeaBIOS
# with its GPL/LGPL texts), the licences of the Rust crates r86 may link
# (script/crate_licenses.py, needs cargo) and SeaBIOS's build configs and
# source location. Used by ports/share.sh, script/build_windows.sh,
# script/build_macos.sh, script/build_ios.sh and script/build_wasm.py;
# POSIX cp, no GNU install.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
licenses=$1
mkdir -p "$licenses/seabios"
cp "$root/LICENSE" "$root/THIRD_PARTY_NOTICES.md" "$licenses/"
python3 -I "$root/script/crate_licenses.py" "$licenses/crates.txt"
cp "$root/firmware/seabios/bios.config" "$root/firmware/seabios/vga.config" \
    "$licenses/seabios/"
cat >"$licenses/seabios/SOURCE" <<'TXT'
bios.bin and vgabios-stdvga.bin are SeaBIOS 1.17.0 (LGPL-3.0), upstream
commit b52ca86e094d19b58e2304417787e96b940e39c6 (https://www.seabios.org/),
with r86's changes, built with the configs in this directory by r86's
script/build_seabios.py. The corresponding source is vendor/seabios in r86's
repository (https://github.com/tcchen2026/r86); vendor/README.md lists the
changes. The licence texts are in ../THIRD_PARTY_NOTICES.md.
TXT
