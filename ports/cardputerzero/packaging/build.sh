#!/bin/sh
# Build step of AppBuilder's scripts/pack-deb.sh
# (https://github.com/CardputerZero/AppBuilder), run from the r86 checkout:
#   path/to/AppBuilder/scripts/pack-deb.sh ports/cardputerzero --output-dir tmp/deb
# Builds the same aarch64 glibc 2.27 binary as TrimUI (cargo-zigbuild; zig
# must be on PATH), so HDMI can load the device's SDL2. Debian 13 has glibc
# 2.41. Also builds the bundled DOS file kit (the first run downloads
# the pinned FreeDOS release media into tmp/freedos/dl).
set -eu
cd "$(dirname "$0")/../../.."
target=aarch64-unknown-linux-gnu
if command -v rustup >/dev/null; then rustup target add "$target"; fi
cargo zigbuild --release --target "$target.2.27" --features handheld-frontend --bin r86-handheld
python3 script/build_freedos_image.py --share-dir tmp/freedos/share
