"""Pinned TinyCore 17.x i386 Lua 5.3.6 and its runtime dependencies.

Archives are external test inputs, extracted under ignored experiments/out.
Their published MD5 values were checked when recording these SHA-256 pins.
"""
import hashlib
from pathlib import Path
import subprocess
import urllib.request

BASE = "https://distro.ibiblio.org/tinycorelinux/17.x/x86/tcz/"
PACKAGES = {
    "lua.tcz": "583d214bf0699f80de34b4d8b6beeb75405c742d53d005f3d3c26bc808e658db",
    "readline.tcz": "4539441be69269e6a45362a2a573e34e3c9a2278939191456fa4dac19fd3f472",
    "ncursesw.tcz": "a489641801ed8a251e7a7aae553f176d4ceef4f4051b43eb022e5cdbc9b37748",
}


def prepare(cache: Path) -> Path:
    cache.mkdir(parents=True, exist_ok=True)
    root = cache / "root"
    for name, digest in PACKAGES.items():
        archive = cache / name
        if not archive.exists():
            with urllib.request.urlopen(BASE + name, timeout=60) as response:
                data = response.read()
            if hashlib.sha256(data).hexdigest() != digest:
                raise ValueError(f"downloaded package does not match pinned hash: {name}")
            archive.write_bytes(data)
        if hashlib.sha256(archive.read_bytes()).hexdigest() != digest:
            raise ValueError(f"cached package does not match pinned hash: {archive}")
        subprocess.run(["unsquashfs", "-no-progress", "-f", "-d", str(root), str(archive)],
                       check=True, stdout=subprocess.DEVNULL)
    return root
