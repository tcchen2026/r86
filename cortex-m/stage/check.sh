#!/bin/sh
# Verification commands of cortex-m/PLAYBOOK.md. Run from the repository root:
#
#   sh cortex-m/stage/check.sh MODE
#
#   stage    staged files match cortex-m/stage/SHA256SUMS
#   host     assembler and block-index unit tests on the build machine
#   arm      block and whole-Machine tests under qemu-arm -cpu cortex-a7
#   arm32    the ARMv6K backend still passes without the feature
#   default  the default host build still compiles without the feature
#   bare     thumbv8m.main-none-eabihf library build
#   fmt      rustfmt on the new module
#   boot     TinyCore boot in both engines; their output must match
#
# Each mode prints its decisive lines and ends with "PASS <mode>" or
# "FAIL <mode>: reason". The complete output is in tmp/t32/log/.
set -u
[ -f Cargo.toml ] && [ -d src/x86/jit ] || { echo "run from the repository root" >&2; exit 2; }
STAGE=$(dirname "$0")
MODE=${1:-}
LOG=tmp/t32/log
mkdir -p "$LOG"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-tmp/target-t32}"
ARM=arm-unknown-linux-musleabihf
# The cross linker and the QEMU runner are set per command below. Inherited
# values would break the host builds or select the wrong CPU model.
unset RUSTFLAGS CARGO_TARGET_ARM_UNKNOWN_LINUX_MUSLEABIHF_RUNNER

pass() {
    echo "PASS $MODE"
    exit 0
}

fail() {
    echo "FAIL $MODE: $1 (log: $LOG)"
    exit 1
}

# No compiler diagnostic may point into the new module.
clean() {
    if grep -a -A4 '^\(warning\|error\)' "$1" | grep -aq 'src/x86/jit/t32/'; then
        grep -a -B4 -A12 'src/x86/jit/t32/' "$1" | head -60
        fail "compiler diagnostics in src/x86/jit/t32"
    fi
}

# tests_ok LOG MINIMUM NAME...: every test passed, at least MINIMUM ran, and
# each NAME (a suffix of the test path) is reported ok.
tests_ok() {
    log=$1
    minimum=$2
    shift 2
    line=$(grep -a '^test result:' "$log" | tail -1)
    echo "$line"
    case $line in
    "test result: ok."*" 0 failed;"*) ;;
    *)
        grep -a 'FAILED\|panicked\|error\|Illegal\|signal' "$log" | head -20
        fail "tests did not all pass"
        ;;
    esac
    passed=$(echo "$line" | sed 's/^test result: ok\. \([0-9]*\) passed.*/\1/')
    [ "$passed" -ge "$minimum" ] || fail "$passed tests passed, expected at least $minimum"
    for name in "$@"; do
        grep -aq "^test .*$name \.\.\. ok" "$log" || fail "test $name did not run or did not pass"
    done
}

case $MODE in
stage)
    (cd "$STAGE" && sha256sum -c SHA256SUMS) >"$LOG/stage.log" 2>&1 || {
        cat "$LOG/stage.log"
        fail "a staged file differs from SHA256SUMS"
    }
    pass
    ;;
host)
    cargo test --release --features jit-t32 --lib t32:: >"$LOG/host.log" 2>&1
    clean "$LOG/host.log"
    tests_ok "$LOG/host.log" 5 \
        asm::tests::encodings_match_llvm_mc \
        asm::tests::branches_resolve_forward_and_backward \
        asm::tests::modified_immediates \
        store::tests::keys_are_exact_and_removal_is_per_record \
        store::tests::a_full_set_replaces_one_record_and_keeps_the_others
    pass
    ;;
arm)
    RUSTFLAGS='-C linker=rust-lld' \
        CARGO_TARGET_ARM_UNKNOWN_LINUX_MUSLEABIHF_RUNNER='qemu-arm -cpu cortex-a7' \
        cargo test --release --target $ARM --features jit-t32 --lib -- t32:: arm_jit \
        >"$LOG/arm.log" 2>&1
    clean "$LOG/arm.log"
    tests_ok "$LOG/arm.log" 17 \
        t32::tests::shared_stub_matches_reference \
        t32::tests::register_alu_matches_interpreter \
        t32::tests::carry_chains_and_partial_flag_writers_match_interpreter \
        t32::tests::conditions_branch_set_and_move_like_the_interpreter \
        t32::tests::shifts_multiplies_and_bit_operations_match_interpreter \
        t32::tests::moves_extensions_and_exchanges_match_interpreter \
        t32::tests::memory_operands_exit_before_the_access_without_a_translation \
        t32::tests::blocks_reserve_their_whole_length_from_the_budget \
        t32::tests::division_matches_interpreter_and_leaves_before_a_fault \
        arm_jit_matches_interpreter_across_budgets_memory_and_code_changes \
        arm_jit_random_programs_match_interpreter \
        arm_jit_chains_follow_remapped_code
    pass
    ;;
arm32)
    RUSTFLAGS='-C linker=rust-lld' \
        CARGO_TARGET_ARM_UNKNOWN_LINUX_MUSLEABIHF_RUNNER='qemu-arm -cpu arm11mpcore' \
        cargo test --release --target $ARM --lib -- arm32:: arm_jit >"$LOG/arm32.log" 2>&1
    tests_ok "$LOG/arm32.log" 11 \
        arm32::tests::encodings_are_armv6k \
        arm32::tests::register_alu_matches_interpreter \
        arm_jit_matches_interpreter_across_budgets_memory_and_code_changes \
        arm_jit_random_programs_match_interpreter \
        arm_jit_chains_follow_remapped_code
    pass
    ;;
default)
    cargo check --release --lib >"$LOG/default.log" 2>&1 || {
        tail -30 "$LOG/default.log"
        fail "the default host build does not compile"
    }
    pass
    ;;
bare)
    cargo build --release --target thumbv8m.main-none-eabihf \
        --no-default-features --features machine,jit,jit-t32 --lib >"$LOG/bare.log" 2>&1 || {
        grep -a -A12 '^error' "$LOG/bare.log" | head -60
        fail "the bare-metal library does not build"
    }
    clean "$LOG/bare.log"
    pass
    ;;
fmt)
    rustfmt --edition 2024 --check src/x86/jit/t32/mod.rs >"$LOG/fmt.log" 2>&1 || {
        head -40 "$LOG/fmt.log"
        fail "rustfmt would change src/x86/jit/t32"
    }
    pass
    ;;
boot)
    ISO="${R86_T32_ISO:-tmp/benchmarks/tinycore_boot/assets/tinycore-marker.iso}"
    [ -f "$ISO" ] || fail "the prepared TinyCore ISO $ISO is missing"
    RUSTFLAGS='-C linker=rust-lld' \
        cargo build --release --target $ARM --features jit-t32 --bin r86-run \
        >"$LOG/boot-build.log" 2>&1 || {
        grep -a -A12 '^error' "$LOG/boot-build.log" | head -60
        fail "the 32-bit ARM CLI does not build"
    }
    BIN="$CARGO_TARGET_DIR/$ARM/release/r86-run"
    for engine in jit interp; do
        echo "booting with --cpu-engine $engine (one to four minutes)"
        timeout 1800 qemu-arm -cpu cortex-a7 "$BIN" --firmware-display serial \
            --iso "$ISO" --bios firmware/seabios/bios.bin \
            --vga-bios firmware/seabios/vgabios-stdvga.bin --mem-mib 128 \
            --cmdline 'console=ttyS0,115200n8 loglevel=3 nozswap nofstab' \
            --cpu-engine $engine --clock deterministic --steps 6000000000 \
            --expect-serial R86_BOOTLOCAL_OK \
            >"$LOG/boot-$engine.out" 2>"$LOG/boot-$engine.err"
        status=$?
        if [ $status -ne 0 ]; then
            tail -c 600 "$LOG/boot-$engine.out" "$LOG/boot-$engine.err"
            fail "--cpu-engine $engine exited with status $status"
        fi
        grep -a 'serial expectation matched' "$LOG/boot-$engine.out" ||
            fail "--cpu-engine $engine did not reach the boot marker"
    done
    grep -a 't32 jit: Stats' "$LOG/boot-jit.out" ||
        fail "the JIT run did not report the Thumb-2 engine"
    # Only the engine's statistics line may differ. Filter both files the
    # same way: grep also normalizes a missing final newline.
    for engine in jit interp; do
        grep -av 't32 jit: Stats' "$LOG/boot-$engine.out" >"$LOG/boot-$engine.cmp"
    done
    cmp "$LOG/boot-jit.cmp" "$LOG/boot-interp.cmp" ||
        fail "the two engines' output differs"
    pass
    ;;
*)
    sed -n '2,15p' "$0"
    exit 2
    ;;
esac
