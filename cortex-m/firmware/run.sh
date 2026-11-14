#!/bin/sh
# Boots TinyCore on QEMU's Cortex-M33 board (MPS3 AN524) with the shared
# interpreter and with the Thumb-2 JIT, and checks each against the x86 host:
# the boot marker must appear after the same number of x86 instructions.
#
#   sh cortex-m/firmware/run.sh [interp] [jit]      (default: both)
#
# Needs refs/TinyCore-current.iso, qemu-system-arm and the Rust target
# thumbv8m.main-none-eabihf. Writes tmp/cortex-m/. About ten minutes.
# "cost" lines count Arm instructions through SysTick under -icount; they are
# instruction counts, not times.
set -eu
[ -f Cargo.toml ] || { echo "run from the repository root" >&2; exit 2; }
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-tmp/target-cortex-m}"
OUT=tmp/cortex-m
LIMIT=3000000000
mkdir -p $OUT
cargo build --release --bin r86chip-machine 2>$OUT/build-host.log
cargo rustc --release --no-default-features --features machine,jit,jit-t32 \
    --bin r86-cortex-m --target thumbv8m.main-none-eabihf -- \
    -C link-arg=-Tcortex-m/firmware/qemu.ld -C relocation-model=static 2>$OUT/build-firmware.log
ELF=$(pwd)/$CARGO_TARGET_DIR/thumbv8m.main-none-eabihf/release/r86-cortex-m
HOST=$CARGO_TARGET_DIR/release/r86chip-machine
# The host prepares guest RAM and runs the reference; --jit only sets a flag.
$HOST refs/TinyCore-current.iso $OUT/interp $LIMIT --reference >$OUT/prepare.log
$HOST refs/TinyCore-current.iso $OUT/jit $LIMIT --jit >>$OUT/prepare.log
steps=$(sed 's/.*"x86_steps":\([0-9]*\).*/\1/' $OUT/interp/host-run.json)
hash=$(sed 's/.*"guest_ram_fnv64":"\([0-9a-f]*\)".*/\1/' $OUT/interp/host-run.json)
echo "host: x86_steps=$steps guest_ram_fnv64=$hash"
for engine in ${@:-interp jit}; do
    # The SSE-200 model gives its first core no FPU unless asked.
    (cd $OUT/$engine && timeout 3600 qemu-system-arm -M mps3-an524 \
        -global sse-200.CPU0_FPU=on -global sse-200.CPU0_DSP=on \
        -display none -serial none -monitor none -icount shift=0 \
        -semihosting-config enable=on,target=native -kernel "$ELF" \
        >../$engine.out 2>../$engine.err) || {
        echo "FAIL $engine: QEMU exit status $?"
        tail -c 400 $OUT/$engine.out $OUT/$engine.err
        exit 1
    }
    grep -a '^result:\|^cost:\|jit: Stats' $OUT/$engine.out
    grep -aq "^result: marker=true x86_steps=$steps " $OUT/$engine.out ||
        { echo "FAIL $engine: the marker did not appear after $steps steps"; exit 1; }
    grep -aq "guest_ram_fnv64=$hash" $OUT/$engine.out && echo "guest RAM equals the host's" ||
        echo "guest RAM differs from the host's"
    python3 cortex-m/firmware/profile.py "$ELF" $OUT/$engine/profile.bin >$OUT/$engine.profile.txt
    echo "PASS $engine (profile: $OUT/$engine.profile.txt)"
done
