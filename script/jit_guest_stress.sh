#!/bin/sh
# Run inside TinyCore with either engine. This uses ordinary processes, tmpfs,
# compression and floating-point arithmetic; no emulator-specific guest API.
# CLI: --initrd-file script/jit_guest_stress.sh:/opt/jit_guest_stress.sh \
#      --bootlocal-command 'sh /opt/jit_guest_stress.sh'
# The final marker is printed only after every child and comparison succeeds.
set -eu

worker() {
    file=/tmp/r86-jit-stress.$1
    for round in 1 2 3 4 5 6 7 8; do
        # Real executable bytes exercise varied page contents and compression
        # branches. Different pages must not all compare equal after reuse.
        cat /bin/busybox /bin/busybox > "$file"
        # Distinguish the concurrent streams and successive page reuse cycles.
        printf '%s:%s\n' "$1" "$round" >> "$file"
        gzip -c "$file" > "$file.gz"
        gunzip -c "$file.gz" > "$file.copy"
        cmp "$file" "$file.copy"
        rm -f "$file" "$file.gz" "$file.copy"
    done
}

pids=
for job in 1 2 3 4; do
    worker "$job" &
    pids="$pids $!"
done
for pid in $pids; do
    wait "$pid"
done
awk 'BEGIN {
    x = 0;
    for (i = 0; i < 20000; i++) x = (x + 3) / 4;
    if (x < 0.999999 || x > 1.000001) exit 1;
    printf "floating-point=%.6f\n", x;
}'
echo R86_JIT_GUEST_STRESS_OK
