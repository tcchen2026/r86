#!/bin/sh
# Build the r86 Rust library for the Mac app: apple/build/rust/libr86.a.
# Run by script/build_macos.sh before compiling Swift; also usable by hand.
set -eu
apple=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$apple")
export PATH="$HOME/.cargo/bin:$PATH"
profile=release
[ "${CONFIGURATION:-Release}" = Debug ] && profile=dev
rust_target_dir=${CARGO_TARGET_DIR:-"$root/target/apple"}
case "$rust_target_dir" in
    /*) ;;
    *) rust_target_dir="$root/$rust_target_dir" ;;
esac
cargo rustc --manifest-path "$root/Cargo.toml" --locked --lib --profile "$profile" \
    --target aarch64-apple-darwin --no-default-features --features apple,jit \
    --crate-type staticlib --target-dir "$rust_target_dir"
dir=release
[ "$profile" = dev ] && dir=debug
mkdir -p "$apple/build/rust"
cp "$rust_target_dir/aarch64-apple-darwin/$dir/libr86.a" "$apple/build/rust/libr86.a"
