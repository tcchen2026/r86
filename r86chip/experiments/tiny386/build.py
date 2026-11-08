#!/usr/bin/env python3
"""Build the reference tiny386 CPU for the same RV32IM ISS as r86.

Freeze reference inputs before patching a private build copy. The existing
MIPS packed-access branch is also selected on RISC-V: ordinary RV loads/stores
trap on misalignment, whereas the reference's default casts assume unaligned
host access. Preserve its computed-goto interpreter and software TLB/flags.
Fault diagnostics only add work to fault exits; no custom chip opcode is used.
"""
import argparse
import difflib
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--pc', action='store_true', help='use the original PC devices and x87/MMX')
    parser.add_argument('--fpu', action='store_true', help='enable the original x87/MMX in CPU-only tests')
    args = parser.parse_args()
    out = args.out.resolve()
    if (out / 'firmware.elf').exists():
        parser.error('use a new output directory to preserve measured binaries')
    source = out / 'reference'
    source.mkdir(parents=True, exist_ok=True)
    hashes = {}
    names = ('i386.c', 'i386.h', 'i386ins.def')
    if args.pc or args.fpu:
        names = [p.name for p in (ROOT / 'refs/tiny386').iterdir() if p.suffix in ('.c', '.h', '.def', '.inc')]
    for name in names:
        path = ROOT / 'refs/tiny386' / name
        hashes[str(path.relative_to(ROOT))] = sha256(path)
        shutil.copy2(path, source / name)
    headers = source / 'libc'
    headers.mkdir(exist_ok=True)
    for path in sorted((ROOT / 'refs/tiny386/wasm/libc').glob('*.h')):
        hashes[str(path.relative_to(ROOT))] = sha256(path)
        shutil.copy2(path, headers / path.name)
    glue = out / 'glue'
    glue.mkdir(exist_ok=True)
    for path in sorted(HERE.iterdir()):
        if path.suffix in ('.S', '.c', '.h', '.rs', '.py'):
            hashes[str(path.relative_to(ROOT))] = sha256(path)
            shutil.copy2(path, glue / path.name)
    linker = ROOT / 'r86chip/firmware/machine.ld'
    hashes[str(linker.relative_to(ROOT))] = sha256(linker)
    shutil.copy2(linker, out / 'machine.ld')
    original = (source / 'i386.c').read_text()
    code = original.replace('#ifdef __mips__', '#if defined(__mips__) || defined(__riscv)')
    # long is 32 bits on this target. The reference's cycle counter is an
    # unsigned modular coordinate here, extended by the caller per batch.
    code = code.replace('\tlong cycle;', '\tunsigned long cycle;')
    needle = '\tTRY(fetch8pf(cpu, &b1));'
    assert code.count(needle) == 1
    code = code.replace(needle, '\tif (!fetch8pf(cpu, &b1)) { ++bench_fetch_faults; return false; }')
    needle = '\tif (!ret) {\n\t\tbool pusherr = false;'
    assert code.count(needle) == 1
    code = code.replace(needle, '\tif (!ret) {\n\t\t++bench_faults;\n\t\tbool pusherr = false;')
    # The host reference reads wall nanoseconds for RDTSC. This bare-metal
    # workload uses the same deterministic instruction-frequency clock as r86.
    start = code.index('static uint64_t get_nticks()')
    end = code.index('\n#define RDTSC()', start)
    code = code[:start] + 'static uint64_t get_nticks() { return bench_clock_ticks(); }\n' + code[end:]
    (source / 'i386.c').write_text(code)
    (out / 'reference.patch').write_text(''.join(difflib.unified_diff(
        original.splitlines(True), code.splitlines(True), fromfile='reference/i386.c', tofile='build/i386.c')))
    if args.pc:
        p = source / 'pc.c'
        text = p.read_text()
        needle = '\tmemset(mem, 0, conf->mem_size);'
        assert text.count(needle) == 1
        changed = text.replace(needle, '\t/* The simulator supplied the initialized direct-kernel RAM image. */')
        p.write_text(changed)
        with (out / 'reference.patch').open('a') as f:
            f.write(''.join(difflib.unified_diff(text.splitlines(True), changed.splitlines(True), fromfile='reference/pc.c', tofile='build/pc.c')))
        # Linux switches channel 0 from periodic ticks to mode-0 deadlines.
        # The reference already models mode-0 count/output reads, but its IRQ
        # updater aborts. Supply one edge per reload in that existing time model.
        # Keep this compatibility edit outside the CPU, and test it separately.
        p = source / 'i8254.c'
        text = p.read_text()
        start = text.index('void i8254_update_irq(PITState *pit)')
        switch = text.index('\tswitch(s->mode) {\n', start)
        insert = switch + len('\tswitch(s->mode) {\n')
        mode0 = '''\tcase 0:
\t\tif (s->gate && !s->last_irq_count && d >= s->count) {
\t\t\ts->last_irq_count = s->count;
\t\t\tif (s->irq != -1) {
\t\t\t\tpit->set_irq(pit->pic, s->irq, 1);
\t\t\t\tpit->set_irq(pit->pic, s->irq, 0);
\t\t\t}
\t\t}
\t\tbreak;
'''
        changed = text[:insert] + mode0 + text[insert:]
        p.write_text(changed)
        with (out / 'reference.patch').open('a') as f:
            f.write(''.join(difflib.unified_diff(text.splitlines(True), changed.splitlines(True), fromfile='reference/i8254.c', tofile='build/i8254.c')))
    flags = ['--target=riscv32-unknown-none-elf', '-march=rv32im', '-mabi=ilp32',
             '-O3', '-g', '-ffreestanding', '-fno-builtin', '-fno-strict-aliasing',
             '-ffunction-sections', '-fdata-sections', '-I' + str(source),
             '-I' + str(headers)]
    if args.pc or args.fpu:
        flags += ['-DI386_ENABLE_FPU', '-DI386_ENABLE_MMX']
    if args.pc:
        flags += ['-DBENCH_PC']
    commands = []
    objects = []
    units = [glue / p for p in ('entry.S', 'cpu.c', 'platform.c')]
    if args.pc or args.fpu:
        units.append(source / 'fpu.c')
    if args.pc:
        units.append(glue / 'pc_main.c')
        units += [source / (p + '.c') for p in ('pc', 'i8259', 'i8254', 'ide', 'vga', 'i8042', 'misc', 'adlib', 'ne2000', 'i8257', 'sb16', 'pcspk', 'fdc', 'pci')]
    for unit in units:
        name = unit.name
        obj = out / (name + '.o')
        # Only the device files use their existing browser/headless platform
        # branches. The CPU keeps I386_OPT2, its ordinary computed-goto loop.
        platform_flags = ['-D__wasm__'] if args.pc and unit.parent == source and name != 'fpu.c' else []
        cmd = ['clang', *flags, *platform_flags, '-c', str(unit), '-o', str(obj)]
        commands.append(cmd)
        subprocess.run(cmd, check=True)
        objects.append(str(obj))
    sysroot = Path(subprocess.check_output(['rustc', '--print', 'sysroot'], text=True).strip())
    builtins = next((sysroot / 'lib/rustlib/riscv32im-unknown-none-elf/lib').glob('libcompiler_builtins-*.rlib'))
    if args.pc or args.fpu:
        libm = next((Path.home() / '.cargo/registry/src').glob('*/libm-0.2.16/src/lib.rs'))
        # Freeze all transitive math inputs, not only the entry file.
        math_source = out / 'libm-source'
        shutil.copytree(libm.parent, math_source, dirs_exist_ok=True)
        for path in sorted(math_source.rglob('*.rs')):
            hashes['libm-0.2.16/src/' + str(path.relative_to(math_source))] = sha256(path)
        cmd = ['rustc', '--crate-name', 'libm', '--crate-type', 'rlib', '--edition=2021',
               '--target=riscv32im-unknown-none-elf', '-O', str(math_source / 'lib.rs'), '-o', str(out / 'libm.rlib')]
        commands.append(cmd)
        subprocess.run(cmd, check=True)
        cmd = ['rustc', '--crate-name', 'tiny_math', '--crate-type', 'staticlib', '--edition=2021',
               '--target=riscv32im-unknown-none-elf', '-O', '-C', 'panic=abort',
               '--extern', 'libm=' + str(out / 'libm.rlib'), str(glue / 'math.rs'), '-o', str(out / 'libtiny_math.a')]
        commands.append(cmd)
        subprocess.run(cmd, check=True)
        objects.append(str(out / 'libtiny_math.a'))
    cmd = ['ld.lld', '-m', 'elf32lriscv', '--gc-sections', '--no-relax',
           '-T', str(out / 'machine.ld'), *objects, str(builtins),
           '-o', str(out / 'firmware.elf')]
    commands.append(cmd)
    subprocess.run(cmd, check=True)
    subprocess.run(['llvm-objcopy', '-O', 'binary', str(out / 'firmware.elf'), str(out / 'firmware.bin')], check=True)
    with (out / 'linked.objdump').open('w') as f:
        subprocess.run(['llvm-objdump', '-d', '--demangle', str(out / 'firmware.elf')], stdout=f, check=True)
    versions = {tool: subprocess.check_output([tool, '--version'], text=True).strip()
                for tool in ('clang', 'rustc', 'ld.lld', 'llvm-objdump')}
    (out / 'build.json').write_text(json.dumps(dict(scope=__doc__, source_hashes=hashes,
        commands=commands, fpu=args.pc or args.fpu, pc=args.pc, isa='RV32IM, no custom instructions',
        versions=versions, compiler_builtins_sha256=sha256(builtins),
        firmware_sha256=sha256(out / 'firmware.elf')), indent=2) + '\n')


if __name__ == '__main__':
    main()
