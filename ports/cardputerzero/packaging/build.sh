#!/bin/sh
# Build step of AppBuilder's scripts/pack-deb.sh
# (https://github.com/CardputerZero/AppBuilder), run from the r86 checkout:
#   path/to/AppBuilder/scripts/pack-deb.sh ports/cardputerzero --output-dir tmp/deb
# Builds a static aarch64 r86-handheld, so the binary does not depend on the
# device's glibc, and the bundled DOS file kit (the first run downloads
# the pinned FreeDOS release media into tmp/freedos/dl).
set -eu
cd "$(dirname "$0")/../../.."
target=aarch64-unknown-linux-musl
if command -v rustup >/dev/null; then rustup target add "$target"; fi
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    cargo build --release --target "$target" --features handheld-frontend --bin r86-handheld
python3 script/build_freedos_image.py --share-dir tmp/freedos/share
