#!/bin/bash
set -euo pipefail

r86_binary=${R86_BINARY:-target/release/r86-run}
tiny_binary=${TINY386_BINARY:-refs/tiny386/tiny386}
reactos_iso=${REACTOS_ISO:-refs/ReactOS.iso}
bios=${BIOS:-firmware/seabios/bios.bin}
vga_bios=${VGA_BIOS:-firmware/seabios/vgabios-stdvga.bin}
display=${DISPLAY_NUMBER:-:99}
r86_cpu=${R86_CPU:-1}
tiny_cpu=${TINY386_CPU:-1}
xvfb_cpu=${XVFB_CPU:-2}
out=${OUT_DIR:-tmp/benchmarks/reactos_livecd_boot_to_langsel/sdl}
benchmark_dir=$(dirname -- "${BASH_SOURCE[0]}")
timeout_seconds=${TIMEOUT_SECONDS:-600}
mkdir -p "$out"

for command in Xvfb xdotool import convert python3; do
    command -v "$command" >/dev/null
done
for file in "$r86_binary" "$tiny_binary" "$reactos_iso" "$bios" "$vga_bios"; do
    [[ -f "$file" ]]
done

tiny_config="$out/tiny386.ini"
python3 "$benchmark_dir/../tiny386_config.py" \
    --iso "$reactos_iso" --bios "$bios" --vga-bios "$vga_bios" >"$tiny_config"
mapfile -t r86_signature < <(python3 "$benchmark_dir/endpoint.py" r86)
mapfile -t tiny_signature < <(python3 "$benchmark_dir/endpoint.py" tiny386)

taskset -c "$xvfb_cpu" Xvfb "$display" -screen 0 1024x768x24 -nolisten tcp \
    >"$out/xvfb.log" 2>&1 &
xvfb_pid=$!
emulator_pid=
cleanup() {
    if [[ -n "$emulator_pid" ]] && kill -0 "$emulator_pid" 2>/dev/null; then
        kill -TERM "$emulator_pid" 2>/dev/null || true
        wait "$emulator_pid" 2>/dev/null || true
    fi
    kill -TERM "$xvfb_pid" 2>/dev/null || true
    wait "$xvfb_pid" 2>/dev/null || true
}
trap cleanup EXIT
sleep 0.25

wait_for_window() {
    local title=$1
    local window=
    for _ in $(seq 1 200); do
        window=$(DISPLAY="$display" xdotool search --name "$title" 2>/dev/null | sed -n '1p' || true)
        [[ -n "$window" ]] && break
        sleep 0.05
    done
    [[ -n "$window" ]]
    printf '%s\n' "$window"
}

finish_sample() {
    local emulator=$1
    local start_ns=$2
    local poll=$3
    local matched=$4
    local end_ns ticks hz
    end_ns=$(date +%s%N)
    ticks=$(awk '{print $14 + $15}' "/proc/$emulator_pid/stat")
    hz=$(getconf CLK_TCK)
    cp "$matched" "$out/$emulator-matched.png"
    printf 'emulator=%s wall_ms=%d cpu_ms=%d poll=%d\n' \
        "$emulator" "$(((end_ns - start_ns) / 1000000))" "$((ticks * 1000 / hz))" "$poll" \
        | tee -a "$out/results.tsv"
    kill -TERM "$emulator_pid" 2>/dev/null || true
    wait "$emulator_pid" 2>/dev/null || true
    emulator_pid=
}

run_r86() {
    local start_ns window poll image pixels
    start_ns=$(date +%s%N)
    DISPLAY="$display" taskset -c "$r86_cpu" "$r86_binary" \
        --clock deterministic \
        --iso "$reactos_iso" \
        --firmware-display vga \
        --bios "$bios" \
        --vga-bios "$vga_bios" \
        --mem-mib 128 \
        --steps 0 \
        --sdl2 \
        --sdl2-scale 1 \
        --sdl2-slice-steps 1000000 \
        >"$out/r86.log" 2>&1 &
    emulator_pid=$!
    window=$(wait_for_window '^r86$')
    poll=0
    image="$out/r86-current.png"
    while kill -0 "$emulator_pid" 2>/dev/null; do
        poll=$((poll + 1))
        import -display "$display" -window "$window" "$image"
        pixels=$(convert "$image" -format "${r86_signature[0]}" info:)
        # Next can precede Cancel's paint. Both buttons must be complete.
        if [[ "$pixels" == "${r86_signature[1]}" ]]; then
            finish_sample r86 "$start_ns" "$poll" "$image"
            return
        fi
        if (( ($(date +%s%N) - start_ns) / 1000000000 >= timeout_seconds )); then
            echo 'r86 timed out before the complete language dialog' >&2
            return 1
        fi
        sleep 0.25
    done
    echo 'r86 exited before the ReactOS language dialog appeared' >&2
    return 1
}

run_tiny386() {
    local start_ns window poll image pixels
    start_ns=$(date +%s%N)
    DISPLAY="$display" taskset -c "$tiny_cpu" "$tiny_binary" "$tiny_config" \
        >"$out/tiny386.log" 2>&1 &
    emulator_pid=$!
    window=$(wait_for_window tiny386)
    poll=0
    image="$out/tiny386-current.png"
    while kill -0 "$emulator_pid" 2>/dev/null; do
        poll=$((poll + 1))
        import -display "$display" -window "$window" "$image"
        pixels=$(convert "$image" -format "${tiny_signature[0]}" info:)
        if [[ "$pixels" == "${tiny_signature[1]}" ]]; then
            finish_sample tiny386 "$start_ns" "$poll" "$image"
            return
        fi
        if (( ($(date +%s%N) - start_ns) / 1000000000 >= timeout_seconds )); then
            echo 'tiny386 timed out before the complete language dialog' >&2
            return 1
        fi
        sleep 0.25
    done
    echo 'tiny386 exited before the ReactOS language dialog appeared' >&2
    return 1
}

if [[ ${BENCH_R86:-1} == 1 ]]; then
    run_r86
fi
if [[ ${BENCH_TINY386:-1} == 1 ]]; then
    run_tiny386
fi
