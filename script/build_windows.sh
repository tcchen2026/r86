#!/bin/sh
# Builds the Windows app (r86-win.exe, src/frontend/win32/) on Linux for
# amd64 and arm64 and zips each with what it loads from beside it: SeaBIOS,
# the r86 tools disc (assets/r86_tools.iso from script/build_tools_iso.py)
# and the licenses.
#
#   script/build_windows.sh [x64] [arm64]    (default: both)
#
# Output: tmp/windows/r86-windows-<arch>/ and its zip. Needs zip and, per
# architecture, its rustup target and toolchain: x64 x86_64-pc-windows-gnu
# with mingw-w64; arm64 aarch64-pc-windows-gnullvm with llvm-mingw, named by
# LLVM_MINGW. llvm-mingw stays off PATH, where its x86_64-w64-mingw32-gcc
# would replace mingw-w64's.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/tmp/windows
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/tmp/target-windows}"

rm -rf "$out"
for arch in ${*:-x64 arm64}; do
    case $arch in
    x64)
        target=x86_64-pc-windows-gnu
        windres=x86_64-w64-mingw32-windres
        objdump=x86_64-w64-mingw32-objdump
        ;;
    arm64)
        llvm=${LLVM_MINGW:?set LLVM_MINGW to the llvm-mingw directory}/bin
        target=aarch64-pc-windows-gnullvm
        windres=$llvm/aarch64-w64-mingw32-windres
        objdump=$llvm/llvm-objdump
        export CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER=$llvm/aarch64-w64-mingw32-clang
        # Otherwise llvm-mingw's unwinder is libunwind.dll, which Windows lacks.
        export CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_RUSTFLAGS="-C target-feature=+crt-static"
        ;;
    *)
        echo "usage: $0 [x64] [arm64]" >&2
        exit 2
        ;;
    esac
    WINDRES=$windres cargo build --release --locked --manifest-path "$root/Cargo.toml" --target "$target" \
        --no-default-features --features win32-frontend --bin r86-win
    exe=$CARGO_TARGET_DIR/$target/release/r86-win.exe
    # Only DLLs every Windows 10 and 11 has; a toolchain runtime DLL would
    # stop the app starting.
    for dll in $("$objdump" -p "$exe" | sed -n 's/^[[:space:]]*DLL Name: //p' | tr A-Z a-z | sort -u); do
        case $dll in
        kernel32.dll | user32.dll | gdi32.dll | comdlg32.dll | shell32.dll | winmm.dll | ws2_32.dll) ;;
        ntdll.dll | oleaut32.dll | msvcrt.dll | api-ms-win-*) ;;
        *) echo "r86-win.exe ($arch) imports $dll, which Windows does not ship" >&2; exit 1 ;;
        esac
    done
    app=$out/r86-windows-$arch
    install -D -m 0755 "$exe" "$app/r86-win.exe"
    install -m 0644 "$root/firmware/seabios/bios.bin" "$root/firmware/seabios/vgabios-stdvga.bin" "$app/"
    if [ -f "$root/assets/r86_tools.iso" ]; then
        install -m 0644 "$root/assets/r86_tools.iso" "$app/"
    else
        echo "assets/r86_tools.iso is missing: the zips lack Install r86 Tools" >&2
    fi
    "$root/ports/licenses.sh" "$app/licenses"
    (cd "$out" && zip -q -r -9 "r86-windows-$arch.zip" "r86-windows-$arch")
    echo "$out/r86-windows-$arch.zip"
done
