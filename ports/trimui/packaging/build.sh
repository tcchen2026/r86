#!/bin/sh
# Builds the TrimUI app folder tmp/trimui/Apps/r86: r86-handheld for aarch64
# glibc 2.27 (cargo-zigbuild; the firmware's libSDL2 is loaded at run time),
# SeaBIOS, the DOS kit and licenses. Needs zig and cargo-zigbuild on PATH.
set -eu
root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
# v2026.11.18-b0dd54 (script/current-tree-version.sh) unless R86_VERSION is set.
R86_VERSION=${R86_VERSION:-$(script/current-tree-version.sh)}
export R86_VERSION
target=aarch64-unknown-linux-gnu
cargo zigbuild --release --target "$target.2.27" --features handheld-frontend --bin r86-handheld
python3 script/build_freedos_image.py --share-dir tmp/freedos/share
app=tmp/trimui/Apps/r86
rm -rf "$app"
install -D -m 0755 "target/$target/release/r86-handheld" "$app/r86-handheld"
install -m 0755 ports/trimui/packaging/launch.sh "$app/"
install -m 0644 ports/trimui/packaging/config.json "$app/"
install -m 0644 ports/cardputerzero/packaging/icon.png "$app/icon.png"
ports/share.sh "$app"
