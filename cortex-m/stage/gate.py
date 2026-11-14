#!/usr/bin/env python3
"""Add the `jit-t32` feature and select the Thumb-2 backend with it.

usage: gate.py REPO_ROOT

Edits Cargo.toml and src/x86/jit/mod.rs. Each edit names the exact text it
replaces and fails if that text is not present exactly once. Running it on an
already edited tree fails at the first edit and changes nothing.
"""
import pathlib
import sys


def edit(path, pairs):
    text = path.read_text()
    for old, new in pairs:
        if text.count(old) != 1:
            sys.exit(f"{path}: expected exactly one {old[:60]!r}, found {text.count(old)}")
        text = text.replace(old, new)
    return text


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = pathlib.Path(sys.argv[1])
    cargo = root / "Cargo.toml"
    module = root / "src/x86/jit/mod.rs"
    if "jit-t32" in cargo.read_text() or "jit-t32" in module.read_text():
        sys.exit("jit-t32 is already present; nothing changed")
    cargo_text = edit(cargo, [(
        'jit-profile = ["jit"]\n',
        'jit-profile = ["jit"]\n'
        "# Thumb-2 backend for Armv7-M/Armv8-M Mainline cores (Cortex-M33 class). On\n"
        "# 32-bit ARM targets it replaces the ARMv6K backend.\n"
        'jit-t32 = ["jit"]\n',
    )])
    module_text = edit(module, [
        (
            "//! backend for the 3DS that shares decoding, flag liveness and code validity.\n",
            "//! backend for the 3DS that shares decoding, flag liveness and code validity.\n"
            "//! `t32` (feature `jit-t32`) is the Thumb-2 backend for Cortex-M class cores;\n"
            "//! on 32-bit ARM targets it replaces `arm32`.\n",
        ),
        (
            '#[cfg(target_arch = "arm")]\npub mod arm32;\n',
            '#[cfg(all(target_arch = "arm", not(feature = "jit-t32")))]\npub mod arm32;\n',
        ),
        (
            "pub mod soft;\n",
            'pub mod soft;\n#[cfg(feature = "jit-t32")]\npub mod t32;\n',
        ),
        (
            '#[cfg(all(feature = "machine", target_arch = "arm"))]\npub use arm32::runtime;\n',
            '#[cfg(all(feature = "machine", target_arch = "arm", not(feature = "jit-t32")))]\n'
            "pub use arm32::runtime;\n"
            '#[cfg(all(feature = "machine", target_arch = "arm", feature = "jit-t32"))]\n'
            "pub use t32::runtime;\n",
        ),
    ])
    cargo.write_text(cargo_text)
    module.write_text(module_text)
    print("Cargo.toml: feature jit-t32 added")
    print("src/x86/jit/mod.rs: arm32 gated off and t32 selected under jit-t32")


if __name__ == "__main__":
    main()
