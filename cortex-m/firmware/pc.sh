#!/bin/sh
# Rough BIOS PC gate on QEMU M33; no board addresses or display frontend.
#   sh cortex-m/firmware/pc.sh prepare   # existing DOS installer + host reference
#   sh cortex-m/firmware/pc.sh jit       # the required M33 gate
#   sh cortex-m/firmware/pc.sh interp    # only to diagnose a failed JIT gate
# PC_RUN_DIR changes the output directory. For an existing raw seed, prepare
# with PC_DISK=/path PC_LIMIT=steps PC_PASS_RULE=frame; no seed copy is made.
# PC_FIRMWARE=/path reuses one measured ELF for both engines without rebuilding.
# PC_KEYS=/path replays a fixed keyboard script; see w95a.keys. Prepare saves
# keys.txt beside the host reference; firmware runs check and reuse that file.
# Reuses one read-only raw seed. Guest writes are discarded by Disk::frozen.
set -eu
[ -f Cargo.toml ] || { echo "run from the repository root" >&2; exit 2; }
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-tmp/target-intern}"
OUT=${PC_RUN_DIR:-tmp/cortex-m/pc}
mkdir -p "$OUT"
mode=${1:-jit}
case "$mode" in
prepare)
    cargo build --release --features mcu --bin r86-run --example dos_disk >"$OUT/build-host.log" 2>&1
    if [ -n "${PC_DISK:-}" ]; then
        disk=$(realpath "$PC_DISK")
        if [ -e "$OUT/disk.img" ]; then
            [ "$(realpath "$OUT/disk.img")" = "$disk" ] || { echo "different seed already in $OUT" >&2; exit 2; }
        else
            ln -s "$disk" "$OUT/disk.img"
        fi
    elif [ ! -e "$OUT/disk.img" ]; then
        if [ ! -e "$OUT/freedos.vhd" ]; then
            "$CARGO_TARGET_DIR/release/examples/dos_disk" "$OUT/freedos.vhd" 32 freedos \
                tmp/freedos/share >"$OUT/create-disk.log" 2>&1
        fi
        qemu-img convert -f vpc -O raw "$OUT/freedos.vhd" "$OUT/disk.img"
    fi
    sha256sum "$OUT/disk.img" \
        firmware/seabios/bios.bin firmware/seabios/vgabios-stdvga.bin >"$OUT/inputs.sha256"
    if [ -z "${PC_DISK:-}" ]; then
        sha256sum "$OUT/freedos.vhd" tmp/freedos/share/dos.zip >>"$OUT/inputs.sha256"
    fi
    set -- prepare --out "$OUT" --host "$CARGO_TARGET_DIR/release/r86-run" --pass-rule "${PC_PASS_RULE:-dos}"
    if [ -n "${PC_LIMIT:-}" ]; then set -- "$@" --limit "$PC_LIMIT"; fi
    if [ -n "${PC_KEYS:-}" ]; then set -- "$@" --keys "$PC_KEYS"; fi
    python3 cortex-m/firmware/pc.py "$@"
    ;;
jit|interp)
    sha256sum -c "$OUT/inputs.sha256"
    set -- config --out "$OUT" --engine "$mode"
    if [ -n "${PC_KEYS:-}" ]; then set -- "$@" --keys "$PC_KEYS"; fi
    python3 cortex-m/firmware/pc.py "$@"
    if [ -n "${PC_FIRMWARE:-}" ]; then
        ELF=$(realpath "$PC_FIRMWARE")
    else
        cargo rustc --release --no-default-features --features mcu,jit-t32 \
            --bin r86-cortex-m --target thumbv8m.main-none-eabihf -- \
            -C link-arg=-Tcortex-m/firmware/qemu.ld -C relocation-model=static \
            >"$OUT/build-firmware.log" 2>&1
        cp "$CARGO_TARGET_DIR/thumbv8m.main-none-eabihf/release/r86-cortex-m" "$OUT/firmware.elf"
        ELF=$(realpath "$OUT/firmware.elf")
    fi
    (cd "$OUT" && timeout 3600 qemu-system-arm -M mps3-an524 \
        -global sse-200.CPU0_FPU=on -global sse-200.CPU0_DSP=on \
        -display none -serial none -monitor none -icount shift=0 \
        -semihosting-config enable=on,target=native -kernel "$ELF" \
        >"$mode.out" 2>"$mode.err") || {
        status=$?
        tail -c 2000 "$OUT/$mode.out" "$OUT/$mode.err"
        echo "FAIL pc $mode: QEMU exit status $status"
        exit "$status"
    }
    mv "$OUT/frame.bin" "$OUT/$mode.frame"
    mv "$OUT/profile.bin" "$OUT/$mode.profile.bin"
    python3 cortex-m/firmware/pc.py check --out "$OUT" --engine "$mode"
    sha256sum -c "$OUT/inputs.sha256"
    ;;
*) echo "usage: pc.sh [prepare|jit|interp]" >&2; exit 2 ;;
esac
