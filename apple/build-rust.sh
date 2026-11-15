#!/bin/sh
# Build the r86 Rust library for the Apple apps; also usable by hand.
#   apple/build-rust.sh                       Mac app: aarch64-apple-darwin with the
#                                             JIT, apple/build/rust/libr86.a
#   apple/build-rust.sh aarch64-apple-ios     iOS device, interpreter only (no JIT
#   apple/build-rust.sh aarch64-apple-ios-sim on iOS), apple/build/rust/<target>/libr86.a
#   apple/build-rust.sh aarch64-apple-watchos     watchOS 26 arm64 device (Series 9+)
#   apple/build-rust.sh aarch64-apple-watchos-sim Watch simulator; interpreter only,
#                                              apple/build/rust/<target>/libr86.a
# Watch builds use stable Rust with rust-src (rustup component add rust-src
# --toolchain stable), RUSTC_BOOTSTRAP=1 and -Zbuild-std=std,panic_abort.
# Run by script/build_macos.sh and script/build_ios.sh before compiling Swift.
set -eu
apple=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$apple")
export PATH="$HOME/.cargo/bin:$PATH"
target=${1:-aarch64-apple-darwin}
set --
case $target in
    aarch64-apple-darwin)
        features=apple,jit
        out="$apple/build/rust"
        ;;
    aarch64-apple-ios | aarch64-apple-ios-sim)
        features=apple
        out="$apple/build/rust/$target"
        export IPHONEOS_DEPLOYMENT_TARGET=26.0
        ;;
    aarch64-apple-watchos | aarch64-apple-watchos-sim)
        features=apple
        out="$apple/build/rust/$target"
        export WATCHOS_DEPLOYMENT_TARGET=26.0
        export RUSTUP_TOOLCHAIN=stable RUSTC_BOOTSTRAP=1
        set -- -Zbuild-std=std,panic_abort
        ;;
    *)
        echo "usage: $0 [aarch64-apple-darwin|aarch64-apple-ios|aarch64-apple-ios-sim|aarch64-apple-watchos|aarch64-apple-watchos-sim]" >&2
        exit 2
        ;;
esac
profile=release
[ "${CONFIGURATION:-Release}" = Debug ] && profile=dev
rust_target_dir=${CARGO_TARGET_DIR:-"$root/target/apple"}
case "$rust_target_dir" in
    /*) ;;
    *) rust_target_dir="$root/$rust_target_dir" ;;
esac
cargo rustc "$@" --manifest-path "$root/Cargo.toml" --locked --lib --profile "$profile" \
    --target "$target" --no-default-features --features "$features" \
    --crate-type staticlib --target-dir "$rust_target_dir"
dir=release
[ "$profile" = dev ] && dir=debug
mkdir -p "$out"
cp "$rust_target_dir/$target/$dir/libr86.a" "$out/libr86.a"
