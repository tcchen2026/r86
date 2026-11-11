#!/bin/sh
# Build the r86 Rust library for the Mac app: apple/build/rust/libr86.a.
# Run by script/build_macos.sh before compiling Swift; also usable by hand.
set -eu
apple=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$apple")
export PATH="$HOME/.cargo/bin:$PATH"
profile=release
[ "${CONFIGURATION:-Release}" = Debug ] && profile=dev
cargo rustc --manifest-path "$root/Cargo.toml" --locked --lib --profile "$profile" \
    --target aarch64-apple-darwin --no-default-features --features apple \
    --crate-type staticlib --target-dir "$root/target/apple"
dir=release
[ "$profile" = dev ] && dir=debug
mkdir -p "$apple/build/rust"
cp "$root/target/apple/aarch64-apple-darwin/$dir/libr86.a" "$apple/build/rust/libr86.a"
