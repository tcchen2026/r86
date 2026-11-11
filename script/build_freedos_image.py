#!/usr/bin/env python3
"""Assemble the FreeDOS setup kit consumed by r86's Rust disk planter.

Output: SHARE/dos.zip holds the kit. Its base/ contains the full FreeDOS package
tree, tools and games, with kernel 2046 and corrected ECM SHARE. The freedos,
win31, win95, win98 and winme folders contain only startup/setup overlays. The planter merges base with
one overlay and creates the selected disk; Python does not format filesystems.
boot/ contains verified MBR, FAT16 and FAT32 templates for the planter to patch.

Inputs: official release media, pinned to the SHA-256 values published in
https://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/distributions/1.4/verify.txt
  FD14-LiveCD.zip   FD14LIVE.ISO supplies every package (PACKAGES/GROUP/NAME.ZIP)
                    and FREEDOS/SETUP/VERSION.FDI.
  FD14-LiteUSB.zip  FD14LITE.IMG supplies MBR and FAT16 boot templates.
  ke2046_86f32.zip  supplies the FAT32-capable 8086 kernel and its original tools.
  ke2046s.zip       supplies corresponding source and license text.
  br_fat32fd_0x52.h supplies the pinned FreeDOS FAT32 LBA bootstrap bytes.
  fdshare.zip      supplies SHARE source, packed executable, help and notices.

Installation follows the FreeDOS installer on the same media:
  - Package directories map as in its FDNPKG.CFG (DIR_MAP below). SOURCE/ is
    copied to the kit's sources/ tree, outside the installed guest tree.
  - SYS step: C:\\KERNEL.SYS is kernel 2046 and C:\\COMMAND.COM is FreeCOM
    (BIN\\COMMAND.COM equals the installer's English CMD-EN.COM). Both files
    retain the release bytes, including the kernel's default configuration.
  - Common startup settings are HIMEMX, DOS=HIGH,NOUMB and FILES=30; all
    profiles report DOS 7.10. Win31 and Win98 use HIMEMX /METHOD:FAST and
    DOS=LOW,NOUMB for DOSX. Win98 skips Setup's false minimum-hardware failure
    under FreeDOS with /NM. Win31's INI step sets InDOSPolling and comments WfW's
    protected file/cache/share drivers when present. Its RUN path loads SHARE immediately
    before WIN; Setup remains separate. SHARE also has an on-demand helper.
    Mouse and CD-ROM drivers load on demand; setup loads CD-ROM as D:.
    The FreeDOS menu also offers JEMMEX and Windows 3.1 setup entries. The latter
    uses switchable HIMEMX, DOS=LOW and SHARE, then ends at the DOS prompt.

Only Python 3's standard library and pinned downloads are needed, including on
arm64 Linux. Original archives, documentation and licenses are retained; the
SHARE executable's sole decoded-byte correction is recorded with its source patch.
It enlarges SHARE's 64-byte private stack, smaller than its verified 92-byte
ordinary INT 2F handler path, to 512 bytes.
Provenance is written to assets/freedos.json and provenance.json in the zip.
Windows media is supplied separately; no Microsoft files are included.

Usage: python3 script/build_freedos_image.py [--share-dir tmp/freedos/share]
The published kit is a GitHub Release asset (tag freedos-kit-<first 8 hex>);
assets/freedos-kit.sha256 pins it. Update the pin when uploading a new kit.
Downloads stay in tmp/freedos/dl and tmp/downloads/{fdkernel,freedos-boot,ecm}.
"""

import argparse
import calendar
import hashlib
import io
import json
import os
import re
import shutil
import struct
import sys
import tempfile
import urllib.request
import zipfile
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "assets" / "freedos.json"

DIST = "https://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/distributions/1.4/"
LIVECD = ("FD14-LiveCD.zip", "2020ff6bb681967fd6eff8f51ad2e5cd5ab4421165948cef4246e4f7fcaf6339")
LITEUSB = ("FD14-LiteUSB.zip", "857dcd2ebf9d3d094320154db5fb5b830acba6fb98f981a95a0ca7ab3350338b")
ISO_MEMBER = "FD14LIVE.iso"
IMG_MEMBER = "FD14LITE.img"
KERNEL2046_URL = "https://github.com/FDOS/kernel/releases/download/ke2046/"
KERNEL2046_BINARY = (
    "ke2046_86f32.zip", "72bc3cf6a075cb2d91679687be3e726eff6f4dcbdd768faf958dd1b9f865c294")
KERNEL2046_SOURCE = (
    "ke2046s.zip", "0ea71c9276aba7e1e371f42e8e17d4e9223ac4bedeacccce3ee700fc2866c5e3")
SHARE_URL = "https://pushbx.org/ecm/download/"
SHARE_ARCHIVE = (
    "fdshare.zip", "aaf91cac096cbb8512901c8e2b4fdb8c02107ec0153268c04dbc65b7eb0790b9")
SHARE_PACKED_SHA = "810174d9bfca4ebbbbcbc8e3d7fa2c8eb5870c560c4173a33edf35025cb3ab0c"
SHARE_UNPACKED_SHA = "b1c11f27c6e1af0149022c8cc70ff38014c8f083a197f6a29caa43b4b46a2ed5"
SHARE_PATCHED_SHA = "051c833da4b07c70dfe4950939e816f3501dbf2de3dabe3e109e132d9eff585a"
SHARE_SOURCE_PATCH = (
    b"--- a/src/share.c\n+++ b/src/share.c\n@@ -1196,7 +1196,7 @@\n"
    b" \t}\n \n #if defined(__GNUC__) || defined(__WATCOMC__)\n"
    b"-\ttop_of_tsr += 4; // Add 64 bytes for stack\n"
    b"+\ttop_of_tsr += 32; // Add 512 bytes for the GCC-ia16 handler stack\n"
    b" \ttop_of_stack = (top_of_tsr << 4);\n \n \tif (FP_SEG(getvect(0x2D)) == 0\n")
# Primary notices for components named in the ECM map, not claimed build revisions.
GCC_NOTICE_URL = (
    "https://raw.githubusercontent.com/tkchia/gcc-ia16/"
    "9a5539544a9095ad35c4f57a23f2b79461032cb2/")
NEWLIB_NOTICE_URL = (
    "https://raw.githubusercontent.com/tkchia/newlib-ia16/"
    "8aa8f6eb7080eb94713c5b3bee4df44de1df109b/")
SHARE_NOTICES = [
    (GCC_NOTICE_URL + "COPYING3", "GPL3.TXT",
     "8ceb4b9ee5adedde47b31e975c1d90c73ad27b6b165a1dcd80c7c545eb65b903"),
    (GCC_NOTICE_URL + "COPYING.RUNTIME", "RUNTIME.TXT",
     "9d6b43ce4d8de0c878bf16b54d8e7a10d9bd42b75178153e3af6a815bdc90f74"),
    (GCC_NOTICE_URL + "libgcc/config/ia16/ia16-preamble.h", "IA16PRE.H",
     "ddc4631294fa0f5a740dfd44b95e3d169204183f4e1d50e0ea06f0b5d3bcf85a"),
    (GCC_NOTICE_URL + "libgcc/crtstuff.c", "CRTSTUFF.C",
     "c87332582a6a000f92ce8bb4567b78c25191a205568310aabe195de2db1b3d06"),
    (NEWLIB_NOTICE_URL + "COPYING.NEWLIB", "NEWLIB.TXT",
     "992848088cfd1a1a7807828dd8623385713aeba2e3c49f81d1bcefcb23645399"),
    (NEWLIB_NOTICE_URL + "COPYING.LIBGLOSS", "LIBGLOSS.TXT",
     "b6080f61e7bb33cf9c4f1eceddd2b9a3cf919711413ef5b0873b5cb08c0823df"),
    (NEWLIB_NOTICE_URL + "libgloss/ia16/call-cvt.h", "CALLCVT.H",
     "ce514bab8eebae1f31bb075339c427d0b05c06ff4e7ee7b47b218a18d2c32df0"),
    (NEWLIB_NOTICE_URL + "libgloss/ia16/dos-environ.S", "ENVIRON.S",
     "4343cdfa47d10fa64e715e14832b3ec3075d82d70c5b2c73a5f2777209631ddd"),
    (NEWLIB_NOTICE_URL + "libgloss/ia16/dos-openr.c", "OPENR.C",
     "ff85c46e6afab36e420873223b137dbad8a1ae031926368c4f684975ac74ecd8"),
    (NEWLIB_NOTICE_URL + "libgloss/ia16/dos-psp.S", "PSP.S",
     "20993e7aca0d96142571df35679b1013a496233254c7d143c40cd828338abda2"),
    (NEWLIB_NOTICE_URL + "newlib/libc/stdlib/getenv.c", "GETENV.C",
     "f08d07b13033d30ea66dd5ead9616d592e151d99f8c82ce0acff26147531b7df"),
    (NEWLIB_NOTICE_URL + "newlib/libc/stdlib/getenv_r.c", "GETENV_R.C",
     "bd2351e6fcf682577229a3a730822c138ace09213d8a55398b329acbc0e29415"),
    (NEWLIB_NOTICE_URL + "newlib/libc/stdlib/nano-mallocr.c", "NANO.C",
     "e55717b011aaa73f1f0246228a22088653254e137cde0bc914fc2dc488a498a5"),
    (NEWLIB_NOTICE_URL + "libgloss/ia16/dos-mtabort.S", "MTABORT.S",
     "95ace9fd6ceffa425cb340065fd66622bfd84d3c8fbbadd3faa2791c83a000ff"),
    ("https://gitlab.com/tkchia/libi86/-/raw/"
     "521a43f42a3daf00b85b62c44390821f532def8f/debian/copyright", "LIBI86.TXT",
     "b1961d231eb4b2f7d6d0eee1f7bfa8a530b88499059fdba9c40e1ec8e89d72cf"),
]
FAT32_HEADER_URL = (
    "https://raw.githubusercontent.com/pbatard/rufus/"
    "842873017f289713a5d5009320c11f31cfee36ed/src/ms-sys/inc/")
FAT32_HEADER = (
    "br_fat32fd_0x52.h", "b001c3bbe329650bb7d60e17a662e70b103a8a852946c05237e51f9a4e8c433e")
FAT32_TEMPLATE_SHA256 = "eb86f238f70d559f3dc66d082fc55c8459c77b4fe9e2fa9d48c440cf4a7c9a74"

# The base\ entries of the release's FREEDOS/SETUP/FDPLBASE.LST, i.e. the
# installer's "plain DOS" set (kernel, shell, standard commands, CTMOUSE,
# SHSUCDX, EDIT, HIMEMX/JEMM, HELP).
BASE = """
kernel freecom jemm himemx fdisk format xcopy attrib chkdsk append assign
choice comp cpidos ctmouse debug defrag deltree devload diskcomp diskcopy
display edit edlin exe2bin fc fdapm find fdxms fdxms286 keyb keyb_lay label
lbacache mem mirror mkeyb mode more move nansi nlsfunc print recover replace
share shsucdx sort swsubst tree undelete unformat graphics fdhelper htmlhelp
""".split()
# CD-ROM driver, INFO-ZIP, DPMI host (also from FDPLBASE.LST), file manager.
EXTRAS = ["drivers/udvd2", "archiver/unzip", "archiver/zip", "tools/cwsdpmi", "apps/doszip"]
# (package, command, executable). A None executable means the package ships
# its own C:\FREEDOS\LINKS\COMMAND.BAT; otherwise one is generated.
GAMES = [
    ("vitetris", "TETRIS", "TETRIS.EXE"),
    ("nge_nibb", "NIBBLES", "NIBBLES.EXE"),
    ("flpybird", "FLPYBIRD", None),
    ("sudoku86", "SUDOKU86", "SUDOKU86.EXE"),
    ("senet", "SENET", None),
]
PACKAGES = ([f"base/{n}" for n in BASE if n not in ("kernel", "share")]
            + EXTRAS + [f"games/{g[0]}" for g in GAMES] + ["unix/gnused"])

# Clarifies Copying-policy fields that are empty or only point at a file.
LICENSE_NOTES = {
    "shsucdx": "zlib License (upstream https://github.com/adoxa/shsucd LICENSE.txt); "
               "the LSM has no Copying-policy, DOC/SHSUCDX/SHSUCDX.TXT says Freeware",
    "udvd2": "no license text: README offers it at no cost, as is; source is in the "
             "package; redistributed on all FreeDOS 1.4 media",
    "unzip": "Info-ZIP License (BSD-style), DOC/UNZIP/LICENSE",
    "zip": "Info-ZIP License (BSD-style), DOC/ZIP/LICENSE",
    "htmlhelp": "viewer: Express Software Public License 1.0 (DOC/HELP/COPYING); "
                "help content: GNU Free Documentation License",
}

# Package top-level directory -> install directory, from the installer's
# FDNPKG.CFG "dir" lines (DOSDIR=C:\FREEDOS). Only directories used by the
# packages above are listed; anything else stops the build.
DIR_MAP = {
    "APPINFO": "FREEDOS/APPINFO", "BIN": "FREEDOS/BIN", "CPI": "FREEDOS/CPI",
    "DOC": "FREEDOS/DOC", "HELP": "FREEDOS/HELP", "NLS": "FREEDOS/NLS",
    "BATLINKS": "FREEDOS/LINKS", "GAMES": "GAMES", "SOURCE": None,
}

SECTOR = 512
STAMP = (2025, 4, 2, 0, 0, 0)  # FreeDOS 1.4 RELEASE date (VERSION.FDI)

# Preallocate 30 SFT slots instead of letting WfW append its own table.
FDCONFIG = r"""
REM FreeDOS 1.4 for r86, generated by script/build_freedos_image.py
SET DOSDIR=C:\FREEDOS
!LASTDRIVE=E
!BUFFERS=10
!FILES=30
VERSION=7.10
DEVICE=C:\FREEDOS\BIN\HIMEMX.EXE
DOS=HIGH,NOUMB
SHELL=C:\FREEDOS\BIN\COMMAND.COM C:\FREEDOS\BIN /E:1024 /P=C:\AUTOEXEC.BAT
"""

DOS_STARTUP = r"""
@ECHO OFF
REM FreeDOS 1.4 for r86, generated by script/build_freedos_image.py
SET DOSDRV=C:
SET DOSDIR=C:\FREEDOS
PATH C:\R86;C:\FREEDOS\BIN;C:\FREEDOS\LINKS
SET NLSPATH=C:\FREEDOS\NLS
SET HELPPATH=C:\FREEDOS\HELP
SET TEMP=C:\FREEDOS\TEMP
SET TMP=C:\FREEDOS\TEMP
SET LANG=EN
SET TZ=UTC
SET BLASTER=A220 I5 D1 H5 T6
SET OS_NAME=FreeDOS
SET OS_VERSION=1.4
SET DIRCMD=/O:GNE /Y
SET COPYCMD=/-Y
ALIAS reboot=fdapm warmboot
ALIAS shutdown=fdapm poweroff
"""

ON_DEMAND = {
    "SHARE.BAT": r"""
@ECHO OFF
C:\FREEDOS\BIN\SHARE.COM %1 %2 %3 %4 %5 %6 %7 %8 %9
""",
    "MOUSE.BAT": r"""
@ECHO OFF
CTMOUSE /P %1 %2 %3 %4 %5 %6 %7 %8 %9
""",
    "CDROM.BAT": r"""
@ECHO OFF
REM /L:0 returns 255 only when SHSUCDX is not installed.
SHSUCDX /QQ /L:0 >NUL
IF NOT ERRORLEVEL 255 GOTO END
DEVLOAD /Q C:\FREEDOS\BIN\UDVD2.SYS /D:FDCDX001 /H
IF ERRORLEVEL 255 GOTO END
SHSUCDX /Q+ /~+ /D:?FDCDX001,D
:END
""",
}

AUTOEXEC = DOS_STARTUP + r"""
IF "%CONFIG%"=="3" CALL C:\R86\SHARE.BAT
ECHO.
ECHO  Welcome to FreeDOS 1.4. Type HELP for the command reference.
ECHO.
ECHO  EDIT text editor    DZ file manager    ZIP, UNZIP archives    MEM memory
ECHO  Games: {games}
ECHO  MOUSE enables the PS/2 mouse. CDROM enables the CD drive as D:.
ECHO  SHARE enables file sharing and locking; SHARE /S displays its status.
ECHO  Type SHUTDOWN to power off.
ECHO.
"""

LINK = r"""
@ECHO OFF
C:
CD \GAMES\{pkg}
{exe} %1 %2 %3 %4 %5 %6 %7 %8 %9
CD \
"""


def die(msg):
    raise SystemExit(f"build_freedos_image: {msg}")


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def file_sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def dos_text(text):
    return text.strip("\n").replace("\n", "\r\n").encode("ascii") + b"\r\n"


def fetch(cache, name, digest, base_url=DIST):
    """Return the cached release file, downloading and verifying it once."""
    path = cache / name
    if not path.exists():
        part = cache / (name + ".part")
        print(f"downloading {base_url}{name}", flush=True)
        with urllib.request.urlopen(base_url + name, timeout=60) as r, open(part, "wb") as out:
            shutil.copyfileobj(r, out, 1 << 20)
        got = file_sha256(part)
        if got != digest:
            part.unlink()
            die(f"{name}: SHA-256 {got}, expected {digest}")
        os.replace(part, path)
    elif file_sha256(path) != digest:
        die(f"{path}: SHA-256 mismatch; delete it and rerun")
    return path


def unpack_share(data: bytes) -> bytes:
    """Decode only the pinned ECM SHARE.COM stream; no general UPX support."""
    if len(data) != 9302 or sha256(data) != SHARE_PACKED_SHA:
        raise ValueError("unexpected packed SHARE.COM")
    cursor, bits, left, distance = 0x3a, 0, 0, 0
    output = bytearray()

    def byte():
        nonlocal cursor
        if cursor >= 0x23f8:
            raise ValueError("truncated SHARE stream")
        value = data[cursor]
        cursor += 1
        return value

    def bit():
        nonlocal bits, left
        if left == 0:
            bits = byte() | (byte() << 8)
            left = 16
        left -= 1
        return (bits >> left) & 1

    while True:
        while bit():
            if len(output) == 12792:
                raise ValueError("SHARE output exceeds its pinned size")
            output.append(byte())
        offset = 1
        while True:
            offset = ((offset << 1) | bit()) & 0xffff
            stop = bit()
            if offset == 0 or stop:
                break
        if offset == 0:
            break
        if offset >= 3:
            distance = ((offset - 3) << 8) + byte() + 1
        length = (bit() << 1) | bit()
        if length == 0:
            length = 1
            while True:
                length = (length << 1) | bit()
                if length > 12792:
                    raise ValueError("invalid SHARE match length")
                if bit():
                    break
            length += 2
        length += 1 + (distance > 0xd00)
        if not 0 < distance <= len(output) or len(output) + length > 12792:
            raise ValueError("invalid SHARE back-reference")
        for _ in range(length):
            output.append(output[-distance])
    if cursor != 0x23f5 or len(output) != 12792:
        raise ValueError("invalid SHARE stream endpoint")

    # Filter 6 reverses 404 E8/E9 operands in the COM image loaded at 0x100.
    cursor = 0
    for _ in range(404):
        while cursor < len(output) and output[cursor] not in (0xe8, 0xe9):
            cursor += 1
        if cursor + 3 > len(output):
            raise ValueError("truncated SHARE branch operand")
        target = int.from_bytes(output[cursor + 1:cursor + 3], "big")
        relative = (target - (cursor + 0x101)) & 0xffff
        output[cursor + 1:cursor + 3] = relative.to_bytes(2, "little")
        cursor += 3
    if sha256(output) != SHARE_UNPACKED_SHA:
        raise ValueError("unexpected unpacked SHARE.COM")
    return bytes(output)


class Iso9660:
    """Primary-volume reader for the files this build needs from FD14LIVE.ISO."""

    def __init__(self, path):
        self.f = open(path, "rb")
        pvd = self._read(16, 2048)
        if pvd[:6] != b"\x01CD001":
            die(f"{path}: no ISO 9660 primary volume descriptor")
        self.root = self._record(pvd, 156)

    def _read(self, lba, size):
        self.f.seek(lba * 2048)
        return self.f.read(size)

    @staticmethod
    def _record(buf, i):
        extent, size = struct.unpack_from("<I4xI", buf, i + 2)
        name = buf[i + 33:i + 33 + buf[i + 32]].decode("ascii").split(";")[0].rstrip(".")
        return name, extent, size

    def read(self, path):
        _, extent, size = self.root
        for part in path.upper().split("/"):
            buf, i, found = self._read(extent, size), 0, None
            while i < len(buf) and not found:
                if buf[i] == 0:  # records never cross a sector boundary
                    i = (i // 2048 + 1) * 2048
                    continue
                rec = self._record(buf, i)
                found = rec if rec[0] == part else None
                i += buf[i]
            if not found:
                die(f"{path}: not on {ISO_MEMBER}")
            _, extent, size = found
        return self._read(extent, size)


def short_name(part):
    """Return the 11-byte FAT directory name, rejecting anything but DOS 8.3."""
    base, _, ext = part.upper().partition(".")
    legal = set("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!#$%&'()-@^_`{}~")
    if not (1 <= len(base) <= 8 and len(ext) <= 3 and set(base + ext) <= legal):
        die(f"{part!r} is not a DOS 8.3 name")
    return (base.ljust(8) + ext.ljust(3)).encode("ascii")


class File:
    def __init__(self, data, stamp):
        self.data, self.stamp = data, stamp


class Tree:
    """Directory tree keyed by upper-case 8.3 names, in insertion order."""

    def __init__(self):
        self.root = {}

    def mkdir(self, path):
        node = self.root
        for part in filter(None, path.upper().split("/")):
            short_name(part)
            node = node.setdefault(part, {})
            if isinstance(node, File):
                die(f"{path}: file in the way")
        return node

    def add(self, path, data, stamp=STAMP):
        parent, _, name = path.upper().rpartition("/")
        node = self.mkdir(parent)
        short_name(name)
        old = node.get(name)
        if old is not None and (not isinstance(old, File) or old.data != data):
            die(f"{path}: installed twice with different contents")
        node[name] = File(data, stamp)

    def get(self, path):
        node = self.root
        for part in path.upper().split("/"):
            node = node[part]
        return node


def lsm_fields(text):
    fields = {}
    for line in text.splitlines():
        key, sep, value = line.partition(":")
        if sep and key == key.strip() and " " not in key:
            fields.setdefault(key.lower(), value.strip())
    return fields


def install(tree, iso, pkg, source_files):
    """Unpack one package as FDNPKG would; return its provenance record."""
    group, name = pkg.split("/")
    iso_path = f"PACKAGES/{group}/{name}.ZIP".upper()
    blob = iso.read(iso_path)
    zf = zipfile.ZipFile(io.BytesIO(blob))
    sources, lsm = [], None
    for zi in zf.infolist():
        parts = [p for p in zi.filename.replace("\\", "/").split("/") if p]
        if any(p in (".", "..") for p in parts):
            die(f"{iso_path}: unsafe member {zi.filename}")
        top = parts[0].upper()
        if top not in DIR_MAP or (len(parts) == 1 and not zi.is_dir()):
            die(f"{iso_path}: no install rule for {zi.filename}")
        if DIR_MAP[top] is None:
            if not zi.is_dir():
                sources.append(zi.filename)
                path = "/".join([group, name] + parts)
                source_files[path] = File(zf.read(zi), zi.date_time)
            continue
        path = "/".join([DIR_MAP[top]] + parts[1:])
        if zi.is_dir():
            tree.mkdir(path)
            continue
        data = zf.read(zi)
        tree.add(path, data, zi.date_time)
        if path.upper() == f"FREEDOS/APPINFO/{name.upper()}.LSM":
            lsm = lsm_fields(data.decode("cp437"))
    if lsm is None:
        die(f"{iso_path}: no APPINFO/{name.upper()}.LSM")
    if name == "gnused":
        # This package keeps its GPL text inside the source archive only.
        with zipfile.ZipFile(io.BytesIO(zf.read("SOURCE/GNUSED/SOURCES.ZIP"))) as src:
            zi = src.getinfo("gnu/sed-4.2.2/COPYING")
            tree.add("FREEDOS/DOC/GNUSED/COPYING", src.read(zi), zi.date_time)
    return {
        "name": name,
        "group": group,
        "title": lsm.get("title"),
        "version": lsm.get("version"),
        "license": lsm.get("copying-policy"),
        "license_note": LICENSE_NOTES.get(name),
        "primary_site": lsm.get("primary-site"),
        "release_media_path": f"{ISO_MEMBER}:{iso_path}",
        "zip_sha256": sha256(blob),
        "sources_in_zip": sources,
        "source_copy_prefix": f"sources/{group}/{name}/",
    }


def install_kernel2046(tree, binary, source):
    """Install the pinned kernel release without the older 1.4 kernel package."""
    with zipfile.ZipFile(binary) as zf:
        lsm = lsm_fields(zf.read("docs/fdkernel.lsm").decode("cp437"))
        for zi in zf.infolist():
            if zi.is_dir():
                continue
            top, name = zi.filename.split("/", 1)
            if top == "docs":
                path = f"FREEDOS/DOC/KERNEL/{name}"
            elif top == "bin" and name in ("kernel.sys", "sys.com", "country.sys", "setver.sys"):
                path = f"FREEDOS/BIN/{name}"
            elif top == "bin":
                path = f"FREEDOS/DOC/KERNEL/SAMPLES/{name}"
            else:
                die(f"{binary.name}: unexpected member {zi.filename}")
            tree.add(path, zf.read(zi), zi.date_time)
        zi = zf.getinfo("docs/fdkernel.lsm")
        tree.add("FREEDOS/APPINFO/KERNEL.LSM", zf.read(zi), zi.date_time)
    with zipfile.ZipFile(source) as zf:
        for member, name in (("SOURCE/ke2046/COPYING", "COPYING"),
                             ("SOURCE/ke2046/country/LICENSE", "COUNTRY.LIC")):
            zi = zf.getinfo(member)
            tree.add(f"FREEDOS/DOC/KERNEL/{name}", zf.read(zi), zi.date_time)
    return {
        "name": "kernel", "group": "base", "title": lsm.get("title"),
        "version": lsm.get("version"), "license": lsm.get("copying-policy"),
        "primary_site": lsm.get("primary-site"),
        "release_archive": binary.name, "zip_sha256": file_sha256(binary),
        "source_archive": source.name, "source_sha256": file_sha256(source),
        "source_url": KERNEL2046_URL + source.name,
        "source_copy": "sources/" + source.name,
        "kernel_member": "bin/kernel.sys",
        "kernel_sha256": sha256(tree.get("FREEDOS/BIN/KERNEL.SYS").data),
    }


def install_share(tree, archive, source_files, runtime_notices):
    """Install a pinned prebuilt SHARE with a source-backed private-stack fix."""
    with zipfile.ZipFile(archive) as zf:
        zi = zf.getinfo("src/share.com")
        try:
            image = bytearray(unpack_share(zf.read(zi)))
        except ValueError as error:
            die(f"{archive.name}: {error}")
        # The shipped 64-byte private stack is smaller than the verified 92-byte
        # ordinary INT 2F handler path. ADD SI,4 becomes ADD SI,32 (512 bytes):
        # both stack top and retained TSR grow.
        if image[0x1d31:0x1d34] != b"\x83\xc6\x04":
            die("SHARE.COM: unexpected stack-reserve instruction")
        image[0x1d33] = 0x20
        if sha256(image) != SHARE_PATCHED_SHA:
            die("SHARE.COM: unexpected patched executable")
        tree.add("FREEDOS/BIN/SHARE.COM", bytes(image), zi.date_time)
        documents = {
            "README.md": "README.TXT", "tnyprntf/LICENSE": "COPYING",
            "kitten/LICENSE": "KITTEN.LIC", "src/amishelp.asm": "AMISHELP.ASM",
            "lmacros/lmacros1.mac": "LMACROS1.MAC",
            "lmacros/lmacros2.mac": "LMACROS2.MAC",
            "lmacros/lmacros3.mac": "LMACROS3.MAC",
        }
        for member, name in documents.items():
            zi = zf.getinfo(member)
            tree.add(f"FREEDOS/DOC/SHARE/{name}", zf.read(zi), zi.date_time)
        for zi in zf.infolist():
            top, _, name = zi.filename.partition("/")
            if top in ("help", "nls") and not zi.is_dir():
                tree.add(f"FREEDOS/{top}/{name}", zf.read(zi), zi.date_time)
        source_sha = sha256(zf.read("src/share.c"))
    source_files["share/" + archive.name] = File(archive.read_bytes(), STAMP)
    source_files["share/stack512-source.patch"] = File(SHARE_SOURCE_PATCH, STAMP)
    notice_records = []
    for name, path, url, digest in runtime_notices:
        data = path.read_bytes()
        guest_path = "FREEDOS/DOC/SHARE/" + name
        source_path = "share/runtime-notices/" + name
        tree.add(guest_path, data)
        source_files[source_path] = File(data, STAMP)
        notice_records.append({"url": url, "sha256": digest, "size": len(data),
                               "installed_file": guest_path,
                               "source_copy": "sources/" + source_path})
    licenses = (
        "Original SHARE, Kitten, tnyprntf, AMIS and macro notices are retained.\n"
        "Additional primary runtime notice snapshots are listed in provenance.json:\n"
        "GCC: GPL3.TXT, RUNTIME.TXT, IA16PRE.H, CRTSTUFF.C.\n"
        "libi86: LIBI86.TXT. Newlib/libgloss: NEWLIB.TXT, LIBGLOSS.TXT and the\n"
        "original IA16, getenv and ARM nano-allocator source notices alongside them.\n"
        "This product includes software developed by the University of California,\n"
        "Berkeley and its contributors.\n"
        "The map/objects identify GCC 6.3.0 and a build-ia16.202501 path, but not exact\n"
        "compiler fork, libi86 or Newlib revisions. These notice snapshots do not claim\n"
        "to be the historical revisions used to build the supplied executable.\n")
    tree.add("FREEDOS/DOC/SHARE/LICENSES.TXT", licenses.encode("ascii"))
    note = (
        "ECM SHARE, identified by the archive and executable hashes in provenance.json.\n"
        "The original archive contains source, objects, map, build recipes and notices.\n"
        "src/share.c implements INT 2F/10A6 used by kernel 2046 for delete/rename.\n"
        "Its 64-byte private stack is smaller than a verified 92-byte ordinary handler path.\n"
        "stack512-source.patch changes the reserve to 512 bytes; both stack top and TSR\n"
        "retained size use that value. The decoded executable changes only byte 0x1D33\n"
        "from 04 to 20. No compiler or external decompressor is used by the kit builder.\n"
        "This is a local correction, not an upstream release or a full historical rebuild.\n"
        "The source patch does not prove every possible nested critical-error stack depth.\n"
        "runtime-notices/ holds primary notices for map-identified runtime components.\n"
        "Their immutable snapshot revisions are not asserted to match the historical build.\n")
    source_files["share/README.txt"] = File(note.encode("ascii"), STAMP)
    return {
        "name": "share", "group": "base", "title": "FreeDOS SHARE (ECM snapshot)",
        "license": "GNU GPL; Kitten LGPL-2.1-or-later; upstream notices retained",
        "release_archive": archive.name, "zip_sha256": SHARE_ARCHIVE[1],
        "primary_site": SHARE_URL + SHARE_ARCHIVE[0],
        "source_copy": "sources/share/" + archive.name,
        "source_member": "src/share.c", "source_sha256": source_sha,
        "packed_member": "src/share.com", "packed_sha256": SHARE_PACKED_SHA,
        "unpacked_sha256": SHARE_UNPACKED_SHA, "installed_sha256": SHARE_PATCHED_SHA,
        "installed_file": "FREEDOS/BIN/SHARE.COM", "installed_bytes": len(image),
        "source_patch": "sources/share/stack512-source.patch",
        "source_patch_sha256": sha256(SHARE_SOURCE_PATCH),
        "binary_patch": {"offset": "0x1D33", "old": "04", "new": "20"},
        "notice_files": ["FREEDOS/DOC/SHARE/" + name for name in documents.values()]
                        + [item["installed_file"] for item in notice_records]
                        + ["FREEDOS/DOC/SHARE/LICENSES.TXT"],
        "runtime_notice_inputs": notice_records,
        "provenance_limit": "Archive metadata and supplied objects/map identify this snapshot; "
                            "no exact historical compiler/runtime rebuild is claimed.",
    }


def configure_win31_setup(tree):
    """Install the DOS-side setup flow; no Windows files are bundled."""
    config = FDCONFIG.replace("FreeDOS 1.4 for r86", "FreeDOS Windows 3.1 setup for r86")
    # DOSX switches A20; keep DOS below 1 MiB and use a switchable HIMEMX method.
    config = config.replace("HIMEMX.EXE\n", "HIMEMX.EXE /METHOD:FAST\n")
    config = config.replace("DOS=HIGH,NOUMB\n", "DOS=LOW,NOUMB\n")
    # FreeCOM /F fails critical CD errors instead of prompting Abort/Retry/Fail.
    config = config.replace(" /E:1024 /P=", " /E:1024 /F /P=")
    tree.add("FDCONFIG.SYS", dos_text(config))
    startup = DOS_STARTUP.replace(r"C:\FREEDOS\LINKS", r"C:\FREEDOS\LINKS;C:\WINDOWS")
    tree.add("AUTOEXEC.BAT", dos_text(startup + r"""
CALL C:\R86\WIN31.BAT
C:
CD \
"""))
    scripts = {
        "R86/WIN31.BAT": r"""
@ECHO OFF
C:
CD \
IF EXIST C:\WINDOWS\WIN.COM GOTO INSTALLED
CALL C:\R86\CDROM.BAT
IF EXIST D:\SETUP.EXE GOTO STAGE
IF EXIST D:\DISK1\SETUP.EXE GOTO STAGE
ECHO Windows 3.1 is not installed. No setup files were found on D:.
ECHO Attach a Windows 3.1 ISO with SETUP.EXE or DISK1 through DISKn folders.
ECHO Then run C:\R86\WIN31.BAT or reboot.
GOTO END
:STAGE
IF EXIST C:\R86\WIN31.OK DEL C:\R86\WIN31.OK >NUL
IF EXIST C:\R86\WIN31.OK GOTO STATEFAIL
CD C:\WINSETUP
IF ERRORLEVEL 1 GOTO MAKEDIR
CD \
REM Delete only this staging tree; DELTREE prompts for root entries even with /Y.
DELTREE /Y C:\WINSETUP\*.* >NUL
IF ERRORLEVEL 2 GOTO COPYFAIL
RD C:\WINSETUP
:MAKEDIR
MD C:\WINSETUP
IF ERRORLEVEL 1 GOTO COPYFAIL
ECHO Copying Windows setup files from D: to C:\WINSETUP ...
SET R86COPYERR=
CALL C:\R86\COPYDISK.BAT D: ROOT
DIR /B /AD /ON D:\DISK* >C:\R86\DISKS.LST
SED -n -r -f C:\R86\DISKS.SED C:\R86\DISKS.LST >C:\R86\DISKS.BAT
IF ERRORLEVEL 1 GOTO COPYFAIL
CALL C:\R86\DISKS.BAT
IF "%R86COPYERR%"=="1" GOTO COPYFAIL
IF NOT EXIST C:\WINSETUP\SETUP.EXE GOTO COPYFAIL
IF NOT EXIST C:\WINSETUP\SETUP.INF GOTO COPYFAIL
CD \WINSETUP
SETUP /H:C:\R86\WIN31.SHH
IF NOT EXIST C:\WINDOWS\WIN.COM GOTO SETUPFAIL
:INSTALLED
IF NOT EXIST C:\WINDOWS\SYSTEM.INI GOTO SETUPFAIL
REM Remove WIN31.OK once when updating these INI compatibility rules.
IF EXIST C:\R86\WIN31.OK GOTO RUN
CALL C:\R86\SETINI.BAT
IF NOT "%R86INI%"=="OK" GOTO END
ECHO Windows INI configured >C:\R86\WIN31.OK
IF EXIST C:\R86\WIN31.OK GOTO RUN
GOTO STATEFAIL
:RUN
IF NOT EXIST C:\FREEDOS\BIN\SHARE.COM GOTO SHAREFAIL
REM SHARE returns 0 when installed, 1 when already resident, 2 or above on failure.
CALL C:\R86\SHARE.BAT
IF ERRORLEVEL 2 GOTO SHAREFAIL
C:
CD \WINDOWS
WIN /3
GOTO END
:COPYFAIL
ECHO Setup files could not be staged. Check the CD and free disk space.
ECHO Windows setup was not started.
GOTO END
:SETUPFAIL
ECHO Windows setup is incomplete: WIN.COM or SYSTEM.INI is missing.
ECHO Complete installation before starting Windows.
GOTO END
:STATEFAIL
ECHO Could not update C:\R86\WIN31.OK. Check its attributes and free disk space.
GOTO END
:SHAREFAIL
ECHO FreeDOS SHARE could not be loaded. Windows was not started.
:END
C:
CD \
SET R86COPYERR=
SET R86INI=
""",
        "R86/COPYDISK.BAT": r"""
@ECHO OFF
REM Nonrecursive binary copy. Later directories in name order replace duplicates.
XCOPY %1\*.* C:\WINSETUP\ /Y /H /R >NUL
IF ERRORLEVEL 2 GOTO FAILED
IF NOT ERRORLEVEL 1 GOTO END
IF "%2"=="ROOT" GOTO END
:FAILED
SET R86COPYERR=1
:END
""",
        "R86/SETINI.BAT": r"""
@ECHO OFF
SET R86INI=
IF NOT EXIST C:\WINDOWS\SYSTEM.INI GOTO MISSING
REM FreeCOM COPY does not report every I/O failure; FC verifies the working copy.
COPY /B /Y C:\WINDOWS\SYSTEM.INI C:\WINDOWS\SYSTEM.NEW >NUL
FC /B C:\WINDOWS\SYSTEM.INI C:\WINDOWS\SYSTEM.NEW >NUL
IF ERRORLEVEL 1 GOTO FAILED
REM FIXINI.SED normalizes lines to CRLF; -b preserves those CR bytes on output.
SED -b -r -i -f C:\R86\FIXINI.SED C:\WINDOWS\SYSTEM.NEW
IF ERRORLEVEL 1 GOTO FAILED
IF NOT EXIST C:\WINDOWS\SYSTEM.NEW GOTO FAILED
FIND /I "[386Enh]" C:\WINDOWS\SYSTEM.NEW >NUL
IF ERRORLEVEL 1 GOTO FAILED
FIND /I "InDOSPolling=TRUE" C:\WINDOWS\SYSTEM.NEW >NUL
IF ERRORLEVEL 1 GOTO FAILED
IF EXIST C:\WINDOWS\SYSTEM.R86 DEL C:\WINDOWS\SYSTEM.R86 >NUL
IF EXIST C:\WINDOWS\SYSTEM.R86 GOTO FAILED
REN C:\WINDOWS\SYSTEM.INI SYSTEM.R86
IF ERRORLEVEL 1 GOTO FAILED
IF EXIST C:\WINDOWS\SYSTEM.INI GOTO FAILED
IF NOT EXIST C:\WINDOWS\SYSTEM.R86 GOTO FAILED
REN C:\WINDOWS\SYSTEM.NEW SYSTEM.INI
IF ERRORLEVEL 1 GOTO RESTORE
IF NOT EXIST C:\WINDOWS\SYSTEM.INI GOTO RESTORE
SET R86INI=OK
GOTO END
:RESTORE
REN C:\WINDOWS\SYSTEM.R86 SYSTEM.INI
ECHO The original SYSTEM.INI is restored or saved as SYSTEM.R86.
GOTO FAILED
:MISSING
ECHO C:\WINDOWS\SYSTEM.INI is missing. Windows was not started.
GOTO END
:FAILED
ECHO SYSTEM.INI could not be updated. Windows was not started.
:END
""",
        # Microsoft WW0653: /H is batch setup; unspecified options use defaults.
        "R86/WIN31.SHH": r"""
[sysinfo]
showsysinfo=no
[configuration]
machine=ibm_compatible
display=vga
mouse=ps2mouse
network=nonet
[windir]
C:\WINDOWS
[userinfo]
"r86 User"
"r86"
[endinstall]
configfiles=save
endopt=exit
""",
    }
    tree.mkdir("WINSETUP")
    for path, text in scripts.items():
        tree.add(path, dos_text(text))
    # sed scripts use LF; FIXINI emits explicit CRLF for existing and new lines.
    # WfW uses DOS file services instead of its protected file/cache/share VxDs.
    # Only matching [386Enh] device lines change; absent drivers are untouched.
    sed_scripts = {
        "R86/FIXINI.SED": r"""
s/\r?$/\r/
/^[ \t]*\[386enh\][ \t]*\r?$/I {
  x
  s/.*/seen/
  x
  a\
InDOSPolling=TRUE\r
}
/^[ \t]*\[386enh\][ \t]*\r?$/I,/^[ \t]*\[/ {
  /^[ \t]*indospolling[ \t]*=/Id
  s@^([ \t]*device[ \t]*=[ \t]*([^ \t\r]*[\\/])?(ifsmgr|vcache|vshare)\.386[ \t]*\r?)$@;\1@I
}
$ {
  x
  /seen/ {
    x
    b
  }
  x
  a\
[386Enh]\r\
InDOSPolling=TRUE\r
}
""",
        # FreeCOM FOR excludes directories; accept only numbered DISK names.
        "R86/DISKS.SED": r"""
s~^[ \t]*(DISK[0-9]+)\r?$~CALL C:\\R86\\COPYDISK.BAT D:\\\1~Ip
""",
    }
    for path, text in sed_scripts.items():
        tree.add(path, text.lstrip("\n").encode("ascii"))


def configure_win9x_setup(tree, directory, title):
    """Run the user's CD setup once and leave subsequent boot changes to it."""
    config = FDCONFIG.replace(" /E:1024 /P=", " /E:1024 /F /P=")
    switches = "/IS /IQ"
    if directory == "WIN98":
        # DOSX needs switchable A20 with no DOS HMA allocation.
        config = config.replace("HIMEMX.EXE\n", "HIMEMX.EXE /METHOD:FAST\n")
        config = config.replace("DOS=HIGH,NOUMB\n", "DOS=LOW,NOUMB\n")
        # FreeDOS triggers a false <16 MiB result, also reproduced in QEMU.
        # /NM skips the CPU, memory and FPU minimum check (Microsoft KB189670).
        switches += " /NM"
    tree.add("FDCONFIG.SYS", dos_text(config))
    startup = DOS_STARTUP.replace("@ECHO OFF\n", "")
    tree.add("AUTOEXEC.BAT", dos_text(r"""
@ECHO OFF
IF EXIST C:\IO.SYS GOTO NATIVE
""" + startup + r"""
IF EXIST C:\R86\SETUP.OK GOTO RESUME
CALL C:\R86\SETUP.BAT
GOTO END
:RESUME
ECHO Setup was already started. Its installer owns subsequent boots.
ECHO To resume manually with the CD attached, run C:\R86\SETUP.BAT.
:END
C:
CD \
:NATIVE
"""))
    script = r"""
@ECHO OFF
SET R86SETUP=
CALL C:\R86\CDROM.BAT
IF EXIST D:\{directory}\SETUP.EXE SET R86SETUP=\{directory}
IF "%R86SETUP%"=="" IF EXIST D:\SETUP.EXE SET R86SETUP=\
IF "%R86SETUP%"=="" GOTO NOMEDIA
ECHO Starting {title} setup from D:%R86SETUP%
{space_note}ECHO Setup attempted >C:\R86\SETUP.OK
IF NOT EXIST C:\R86\SETUP.OK GOTO STATEFAIL
D:
CD %R86SETUP%
SETUP {switches}
GOTO END
:NOMEDIA
ECHO No {title} SETUP.EXE was found on D: or D:\{directory}.
ECHO Attach the installation CD and run C:\R86\SETUP.BAT.
GOTO END
:STATEFAIL
ECHO Could not record the setup attempt. Check free disk space.
:END
C:
CD \
SET R86SETUP=
"""
    note = ("ECHO Windows ME may need a disk larger than 512 MiB. Setup checks free space.\n"
            if directory == "WIN9X" else "")
    tree.add("R86/SETUP.BAT", dos_text(script.format(
        directory=directory, title=title, space_note=note, switches=switches)))


def write_tree(tree, destination):
    """Write one package tree with deterministic DOS timestamps."""
    entries = []

    def write(node, relative):
        path = destination / relative
        path.mkdir(parents=True, exist_ok=True)
        for name, child in sorted(node.items()):
            child_relative = relative / name
            child_path = destination / child_relative
            if isinstance(child, File):
                child_path.write_bytes(child.data)
                stamp = calendar.timegm(child.stamp + (0, 0, 0))
                os.utime(child_path, (stamp, stamp))
                entries.append(f"{child_relative.as_posix()}\0{sha256(child.data)}\0{child.stamp}")
            else:
                write(child, child_relative)
        stamp = calendar.timegm(STAMP + (0, 0, 0))
        os.utime(path, (stamp, stamp))

    write(tree.root, Path())
    return {"files": len(entries), "tree_sha256": sha256("\n".join(entries).encode("ascii"))}


def pack(stage, destination):
    """Write the staged kit as one deflated zip: sorted members, fixed dates."""
    partial = destination.with_name(destination.name + ".part")
    with zipfile.ZipFile(partial, "w") as archive:
        for path in sorted(stage.rglob("*")):
            name = path.relative_to(stage).as_posix()
            if path.is_dir():
                archive.writestr(zipfile.ZipInfo(name + "/", STAMP), b"")
            else:
                info = zipfile.ZipInfo(name, STAMP)
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, path.read_bytes(), compresslevel=9)
    os.replace(partial, destination)


def boot_code(liteusb):
    """Extract the release MBR bootstrap and FAT16 sector without formatting."""
    with zipfile.ZipFile(liteusb) as z, z.open(IMG_MEMBER) as f:
        mbr = f.read(SECTOR)
        start = struct.unpack_from("<I", mbr, 446 + 8)[0]
        f.read((start - 1) * SECTOR)
        vbr = f.read(SECTOR)
    if (mbr[510:] != b"\x55\xaa" or mbr[446 + 4] not in (0x04, 0x06)
            or vbr[:3] != b"\xeb\x3c\x90" or vbr[0x36:0x3E] != b"FAT16   "
            or vbr[0x1F1:0x1FC] != b"KERNEL  SYS" or vbr[510:] != b"\x55\xaa"):
        die(f"{IMG_MEMBER}: unexpected MBR or FAT16 boot sector")
    # The planter owns disk identity and the partition table.
    return mbr[:440] + bytes(70) + b"\x55\xaa", vbr, start


def fat32_boot_code(header):
    """Extract FreeDOS's LBA bootstrap bytes from the pinned data header."""
    text = header.read_text()
    match = re.search(r"unsigned char br_fat32_0x52\[\] = \{(.*?)\};", text, re.S)
    if not match:
        die(f"{header.name}: unexpected array declaration")
    data = bytes(int(n, 16) for n in re.findall(r"0x([0-9a-fA-F]{2})", match.group(1)))
    if len(data) != 918:
        die(f"{header.name}: expected 918 bytes including its FSInfo tail")
    template = b"\xeb\x58\x90" + bytes(0x52 - 3) + data[:512 - 0x52]
    if sha256(template) != FAT32_TEMPLATE_SHA256:
        die(f"{header.name}: FAT32 bootstrap hash mismatch")
    return template


def ensure_iso(livecd, cache):
    """Check the derived ISO against the CRC in the pinned release archive."""
    path = cache / ISO_MEMBER
    with zipfile.ZipFile(livecd) as z:
        zi = z.getinfo(ISO_MEMBER)
        good = path.exists() and path.stat().st_size == zi.file_size
        if good:
            crc = 0
            with open(path, "rb") as f:
                for block in iter(lambda: f.read(1 << 20), b""):
                    crc = zlib.crc32(block, crc)
            good = crc == zi.CRC
        if not good:
            part = cache / (ISO_MEMBER + ".part")
            with z.open(ISO_MEMBER) as src, open(part, "wb") as dst:
                shutil.copyfileobj(src, dst, 1 << 20)  # zipfile checks the CRC
            os.replace(part, path)
    return path


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--share-dir", type=Path, default=ROOT / "tmp" / "freedos" / "share",
                    help="directory of the generated dos.zip kit")
    args = ap.parse_args()
    share_dir = args.share_dir.resolve()
    kit = share_dir / "dos.zip"
    share_dir.mkdir(parents=True, exist_ok=True)
    cache = ROOT / "tmp" / "freedos" / "dl"
    kernel_cache = ROOT / "tmp" / "downloads" / "fdkernel"
    boot_cache = ROOT / "tmp" / "downloads" / "freedos-boot"
    share_cache = ROOT / "tmp" / "downloads" / "ecm"
    for path in (cache, kernel_cache, boot_cache, share_cache):
        path.mkdir(parents=True, exist_ok=True)
    livecd = fetch(cache, *LIVECD)
    liteusb = fetch(cache, *LITEUSB)
    binary = fetch(kernel_cache, *KERNEL2046_BINARY, base_url=KERNEL2046_URL)
    source = fetch(kernel_cache, *KERNEL2046_SOURCE, base_url=KERNEL2046_URL)
    share_archive = fetch(share_cache, *SHARE_ARCHIVE, base_url=SHARE_URL)
    runtime_notices = []
    for url, name, digest in SHARE_NOTICES:
        directory, _, filename = url.rpartition("/")
        notice_cache = share_cache / "notices" / name
        notice_cache.mkdir(parents=True, exist_ok=True)
        path = fetch(notice_cache, filename, digest, base_url=directory + "/")
        runtime_notices.append((name, path, url, digest))
    header = fetch(boot_cache, *FAT32_HEADER, base_url=FAT32_HEADER_URL)
    iso = Iso9660(ensure_iso(livecd, cache))
    mbr, fat16, partition_start = boot_code(liteusb)
    boot = {"mbr.bin": mbr, "fat16.bin": fat16, "fat32.bin": fat32_boot_code(header)}

    base, source_files = Tree(), {}
    packages = [install_kernel2046(base, binary, source)]
    packages += [install(base, iso, pkg, source_files) for pkg in PACKAGES]
    packages.append(install_share(base, share_archive, source_files, runtime_notices))
    base.add("FREEDOS/VERSION.FDI", iso.read("FREEDOS/SETUP/VERSION.FDI"))
    base.mkdir("FREEDOS/TEMP")
    kernel = base.get("FREEDOS/BIN/KERNEL.SYS")
    shell = base.get("FREEDOS/BIN/COMMAND.COM")
    base.add("KERNEL.SYS", kernel.data, kernel.stamp)
    base.add("COMMAND.COM", shell.data, shell.stamp)
    generated = []
    for name, text in ON_DEMAND.items():
        path = f"R86/{name}"
        base.add(path, dos_text(text))
        generated.append("base/" + path)
    for pkg, command, exe in GAMES:
        if exe is None:
            base.get(f"FREEDOS/LINKS/{command}.BAT")
        else:
            base.get(f"GAMES/{pkg}/{exe}")
            path = f"FREEDOS/LINKS/{command}.BAT"
            base.add(path, dos_text(LINK.format(pkg=pkg.upper(), exe=exe)))
            generated.append("base/" + path)

    overlays = {name: Tree() for name in ("freedos", "win31", "win95", "win98", "winme")}
    playing_config = FDCONFIG.replace(
        "DEVICE=C:\\FREEDOS\\BIN\\HIMEMX.EXE\nDOS=HIGH,NOUMB\n", r"""
MENU 1 - DOS (HIMEMX)
MENU 2 - EMS (JEMMEX)
MENU 3 - Windows 3.1 setup
MENUDEFAULT=1,2
1?DEVICE=C:\FREEDOS\BIN\HIMEMX.EXE
1?DOS=HIGH,NOUMB
REM Bundled SeaBIOS/VGA: fixed frame below DA000; scan shadow RAM for UMBs.
2?DEVICE=C:\FREEDOS\BIN\JEMMEX.EXE I=CA00-D9FF FRAME=CA00 S=DA00-EFFF
2?DOS=HIGH,UMB
3?DEVICE=C:\FREEDOS\BIN\HIMEMX.EXE /METHOD:FAST
3?DOS=LOW,NOUMB
""".lstrip("\n"))
    overlays["freedos"].add("FDCONFIG.SYS", dos_text(playing_config))
    overlays["freedos"].add("AUTOEXEC.BAT", dos_text(AUTOEXEC.format(
        games="  ".join(g[1] for g in GAMES))))
    configure_win31_setup(overlays["win31"])
    for name, directory, title in (("win95", "WIN95", "Windows 95"),
                                    ("win98", "WIN98", "Windows 98"),
                                    ("winme", "WIN9X", "Windows ME")):
        configure_win9x_setup(overlays[name], directory, title)

    boot_package = iso.read("PACKAGES/BASE/KERNEL.ZIP")
    with zipfile.ZipFile(io.BytesIO(boot_package)) as z:
        boot_source = z.read("SOURCE/KERNEL/SOURCES.ZIP")
    with zipfile.ZipFile(io.BytesIO(boot_source)) as z:
        boot_license = z.read("COPYING")
        boot16_source_sha = sha256(z.read("boot/boot.asm"))
        boot32_source_sha = sha256(z.read("boot/boot32lb.asm"))
    fdisk_source = source_files["base/fdisk/SOURCE/FDISK/SOURCES.ZIP"].data
    with zipfile.ZipFile(io.BytesIO(fdisk_source)) as z:
        mbr_source_sha = sha256(z.read("BOOTNORM.ASM"))
    source_files["ke2046s.zip"] = File(source.read_bytes(), STAMP)
    source_files["boot/fd14-kernel-sources.zip"] = File(boot_source, STAMP)
    source_files["boot/" + header.name] = File(header.read_bytes(), STAMP)

    downloads = []
    for directory, url, specs, checksum in (
            (cache, DIST, (LIVECD, LITEUSB), DIST + "verify.txt"),
            (kernel_cache, KERNEL2046_URL, (KERNEL2046_BINARY, KERNEL2046_SOURCE),
             "https://github.com/FDOS/kernel/releases/expanded_assets/ke2046"),
            (share_cache, SHARE_URL, (SHARE_ARCHIVE,), "Pinned ECM archive snapshot"),
            (boot_cache, FAT32_HEADER_URL, (FAT32_HEADER,), FAT32_HEADER_URL + header.name)):
        for name, digest in specs:
            downloads.append({"url": url + name, "sha256": digest,
                              "size": (directory / name).stat().st_size,
                              "checksum_source": checksum})
    downloads += [{"url": url, "sha256": digest, "size": path.stat().st_size,
                   "checksum_source": "Pinned primary runtime notice snapshot"}
                  for _, path, url, digest in runtime_notices]

    with tempfile.TemporaryDirectory(prefix=".dos-kit-", dir=share_dir) as tmp:
        stage = Path(tmp) / "dos"
        stage.mkdir()
        base_record = write_tree(base, stage / "base")
        for name, tree in overlays.items():
            write_tree(tree, stage / name)
        (stage / "boot").mkdir()
        for name, data in boot.items():
            (stage / "boot" / name).write_bytes(data)
        for name, file in source_files.items():
            path = stage / "sources" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(file.data)
            stamp = calendar.timegm(file.stamp + (0, 0, 0))
            os.utime(path, (stamp, stamp))
        (stage / "licenses").mkdir()
        (stage / "licenses" / "boot-COPYING").write_bytes(boot_license)
        record = {
            "comment": "Generated by script/build_freedos_image.py; FreeDOS kit provenance.",
            "format": "r86-freedos-kit-1",
            "freedos": {"version": "1.4", "release": "2025-04-02",
                        "kernel": "ke2046 86f32 bin/kernel.sys, unmodified release bytes",
                        "kernel_sha256": sha256(base.get("KERNEL.SYS").data),
                        "shell": "FreeCOM 0.86a (English)"},
            "kernel_selection": {
                "omitted_archive": "ke2046_386f32.zip",
                "omitted_archive_sha256":
                    "9305ba9140c6ab0287612b5898c250756c6c6953204c3bda802b39ef853d372c",
                "source": "ke2046s.zip:SOURCE/ke2046/kernel/int2f.asm",
                "reason": [
                    "The Watcom 386 IntDosCal path saves FS in SI at line 219, but "
                    "SwitchToInt2fStack overwrites SI with caller SS at line 47; line 233 "
                    "then restores the wrong FS.",
                    "An empty-CD FindFirst trace in r86 observed FS change from 1666h to "
                    "00DAh across INT 2F/1206. SHSUCDX returned CF=1/AX=0015h correctly; "
                    "the kernel wrote the error result through the wrong FS and returned "
                    "CF=0 with original AX=4E2Bh to FreeCOM.",
                    "A fresh QEMU boot independently reproduced the false setup-media "
                    "detection with the 386 kernel and no SHARE.",
                    "The unmodified 86f32 release omits this XCPU>=386 FS/GS wrapper. "
                    "No kernel patch or startup media guard is applied.",
                ],
            },
            "downloads": downloads,
            "boot_templates": {
                "mbr": {"file": "boot/mbr.bin", "sha256": sha256(mbr),
                        "source": f"{IMG_MEMBER} bytes 0-439; zero disk metadata; 55AA signature",
                        "planter": "Keep bytes 0-439; supply disk identity and partition table.",
                        "source_archive": "sources/base/fdisk/SOURCE/FDISK/SOURCES.ZIP",
                        "source_member": "BOOTNORM.ASM", "source_sha256": mbr_source_sha,
                        "license": "GPL-2.0; base/FREEDOS/APPINFO/FDISK.LSM; "
                                   "licenses/boot-COPYING"},
                "fat16": {"file": "boot/fat16.bin", "sha256": sha256(fat16),
                          "source": f"{IMG_MEMBER} sector {partition_start}",
                          "planter": "Keep bytes 0-2 and 62-511; replace the BPB.",
                          "source_archive": "sources/boot/fd14-kernel-sources.zip",
                          "source_member": "boot/boot.asm", "source_sha256": boot16_source_sha,
                          "source_package_sha256": sha256(boot_package),
                          "license": "GPL-2.0-or-later; licenses/boot-COPYING"},
                "fat32": {"file": "boot/fat32.bin", "sha256": FAT32_TEMPLATE_SHA256,
                          "source": "Pinned br_fat32fd_0x52.h first 430 array bytes at offset "
                                    "0x52, EB5890 jump and zero BPB; FSInfo tail excluded.",
                          "planter": "Keep bytes 0-2 and 90-511; replace BPB including FAT32 ID.",
                          "source_archive": "sources/boot/fd14-kernel-sources.zip",
                          "source_member": "boot/boot32lb.asm",
                          "source_sha256": boot32_source_sha,
                          "source_package_sha256": sha256(boot_package),
                          "license": "GPL-2.0-or-later; licenses/boot-COPYING"},
            },
            "sources": "Every installed release-package SOURCE member is preserved under its "
                       "source_copy_prefix. sources/ke2046s.zip contains the kernel source; "
                       "sources/boot holds the bootstrap source archive and pinned data header. "
                       "Original package documentation and license files remain in base.",
            "startup": {
                "default": "Playing profile: HIMEMX; DOS=HIGH,NOUMB; VERSION=7.10; "
                           "FILES=30; BUFFERS=10; "
                           "LASTDRIVE=E. No mouse, CD-ROM or power-management TSR autoload.",
                "playing_menu": "FreeDOS profile only: default 1 (HIMEMX), 2-second delay; "
                                "optional 2 (JEMMEX with EMS and UMB), "
                                "3 (switchable HIMEMX, DOS=LOW and SHARE; DOS prompt).",
                "win98_setup": "HIMEMX /METHOD:FAST; DOS=LOW,NOUMB for DOSX. "
                               "SETUP /IS /IQ /NM skips the false minimum-hardware failure "
                               "under FreeDOS; /NM bypasses CPU, memory and FPU checks "
                               "(Microsoft KB189670).",
                "ems_firmware_limit": "The optional CA00-D9FF frame is for the bundled "
                                      "SeaBIOS with stdvga or handheld VGA ROM layouts. "
                                      "S=DA00-EFFF scans writable "
                                      "shadow RAM outside detected ROMs for UMBs. JEMMEX's "
                                      "non-16KiB-aligned frame warning remains visible.",
                "on_demand": "MOUSE.BAT loads CTMOUSE for PS/2. CDROM.BAT loads UDVD2 /H "
                             "and SHSUCDX as D:; setup calls it only when installation is needed. "
                             "SHARE.BAT loads the pinned SHARE.COM and forwards its options.",
            },
            "share_compatibility": [
                "FreeDOS 1.4 SHARE lacks INT 2F/10A6 required by kernel 2046 for delete/rename.",
                "The pinned ECM source implements INT 2F/10A6. Its shipped executable uses "
                "a 64-byte private stack for a verified 92-byte ordinary handler path.",
                "The builder decodes only that pinned executable, increases the reserve to "
                "512 bytes with one byte change, and verifies the full result hash.",
                "The original source/archive, minimal source patch and notices are retained. "
                "No compiler or external decompressor is downloaded or run.",
            ],
            "generated_files": generated,
            "packages": packages,
            "base": base_record,
        }
        provenance = json.dumps(record, indent=2) + "\n"
        (stage / "provenance.json").write_text(provenance)
        pack(stage, kit)
        MANIFEST.parent.mkdir(parents=True, exist_ok=True)
        MANIFEST.write_text(provenance)
    print(f"{kit}: {base_record['files']} shared FreeDOS files")
    print(f"{MANIFEST}: {base_record['tree_sha256']}")


if __name__ == "__main__":
    sys.exit(main())
