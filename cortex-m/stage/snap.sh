#!/bin/sh
# Save or restore the four things the Thumb-2 playbook changes, without Git.
# usage (from the repository root): sh cortex-m/stage/snap.sh save NAME
#                                   sh cortex-m/stage/snap.sh restore NAME
set -eu
[ $# -eq 2 ] || { echo "usage: snap.sh save|restore NAME" >&2; exit 2; }
[ -f Cargo.toml ] && [ -d src/x86/jit ] || { echo "run from the repository root" >&2; exit 2; }
D=tmp/t32/snap/$2
case $1 in
save)
    rm -rf "$D"
    mkdir -p "$D"
    cp Cargo.toml "$D/Cargo.toml"
    cp src/x86/jit/mod.rs "$D/jit-mod.rs"
    cp design/VALIDATION.md "$D/VALIDATION.md"
    if [ -d src/x86/jit/t32 ]; then cp -r src/x86/jit/t32 "$D/t32"; fi
    echo "saved $D"
    ;;
restore)
    [ -f "$D/Cargo.toml" ] || { echo "no snapshot $D" >&2; exit 1; }
    cp "$D/Cargo.toml" Cargo.toml
    cp "$D/jit-mod.rs" src/x86/jit/mod.rs
    cp "$D/VALIDATION.md" design/VALIDATION.md
    rm -rf src/x86/jit/t32
    if [ -d "$D/t32" ]; then cp -r "$D/t32" src/x86/jit/t32; fi
    echo "restored $D"
    ;;
*)
    echo "usage: snap.sh save|restore NAME" >&2
    exit 2
    ;;
esac
