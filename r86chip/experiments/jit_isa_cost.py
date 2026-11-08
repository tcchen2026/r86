#!/usr/bin/env python3
"""Compare saved/current JIT emitters on their matching zero-wait RTL cores.

Builds small native images from actual emitted blocks; the current gold ISS
checks both revisions. Compare guest frames, every RTL retirement/bus event,
emitted size and cycles. Linked objdumps are retained for inspection. Repeated
1/16-instruction blocks separate fixed handoff costs from marginal operation
costs. This excludes compilation, cache misses and whole-machine execution.
"""
import argparse
import hashlib
import json
from pathlib import Path
import struct

import rtl


def emitted(emitter, out, library):
    source = '''extern crate alloc;
pub use r86::x86;
#[path = EMITTER_PATH]
#[allow(dead_code)]
mod compile;
fn main() {
    let cases: &[(&str, &[u8], u32)] = &[
        ("mov32", &[0x89, 0xd8], 0x803),
        ("mov8", &[0x88, 0xd8], 0x803),
        ("mov_high8", &[0x88, 0xdc], 0x803),
        ("mov16", &[0x66, 0x89, 0xd8], 0x803),
        ("lea32_scale4", &[0x8d, 0x44, 0x8b, 4], 0x803),
        ("lea16_scale4", &[0x66, 0x8d, 0x44, 0x8b, 4], 0x803),
        ("jne_taken", &[0x75, 0xfc], 0x803),
        ("jne_untaken", &[0x75, 0xfc], 0x843),
    ];
    for &(name, bytes, flags) in cases {
        for count in [1, 16] {
            if name.starts_with("jne") && count != 1 { continue; }
            let block = compile::block(&bytes.repeat(count), 0x1000, count).unwrap();
            println!(r#"{{"case":"{}","count":{},"guest":{},"source_bytes":{},"flags":{},"words":{:?}}}"#,
                name, count, block.instructions, block.source_bytes, flags, block.words);
        }
    }
}
'''.replace("EMITTER_PATH", json.dumps(str(emitter)))
    driver = out / "emit.rs"
    driver.write_text(source)
    rtl.run(["rustc", "--edition=2024", "-C", "panic=abort", driver, "--extern", f"r86={library}",
             "-L", f"dependency={library.parent / 'deps'}", "-o", out / "emit"])
    return [json.loads(line) for line in rtl.run([out / "emit"]).splitlines()]


def measure(blocks, directory, executable):
    rows = []
    assembly = ['.section .text,"ax",@progbits', '.global _start', '_start:']
    for block in blocks:
        name = f"{block['case']}_{block['count']}"
        words = block.pop("words")
        assembly += [f".global {name}", f"{name}:"] + [f".word 0x{word:08x}" for word in words]
        p = rtl.Program()
        p.li(10, 0x2000); p.li(11, 0x8000)
        p.emit(rtl.it(0x7b, 31, 7, 11, 0), rtl.it(0x7b, 0, 4, 10, 0))
        p.finish()
        struct.pack_into(f"<{len(words)}I", p.mem, 0x2000, *words)
        struct.pack_into("<11I", p.mem, 0x8000,
                         0x807f12a5, 0x81234567, 0xffee1234, 0x1234abcd,
                         0x88888888, 0xeeeeffff, 0xffff0001, 0xabcdef01,
                         block['flags'], 0x1000, 0)
        image = bytes(p.mem)
        path = directory / f"{name}.hex"
        path.write_text("\n".join(f"{b:02x}" for b in image) + "\n")
        path.with_suffix(".bin").write_bytes(image)
        expected, memory = rtl.reference(image, 2)
        rtl.compare_iss(path, expected, memory)
        log = rtl.run([executable, f"+IMAGE={path}", "+LATENCY=0", "+SEED=1"])
        path.with_suffix(".log").write_text(log)
        actual = [(line[0], *[int(v, 16) for v in line.split()[1:]])
                  for line in log.splitlines() if line.startswith(("R ", "D ", "S ", "T "))]
        assert actual == expected, name
        cycles, retired, *_ = map(int, next(s for s in log.splitlines() if s.startswith("C ")).split()[1:])
        rows.append(dict(**block, emitted_words=len(words), retired_native=retired, cycles=cycles,
                         frame=list(struct.unpack_from("<11I", memory, 0x8000)),
                         image_sha256=hashlib.sha256(image).hexdigest()))
    path = directory / "blocks.S"
    path.write_text("\n".join(assembly) + "\n")
    rtl.run(["llvm-mc", "-triple=riscv32", "-mattr=+m,+zba", "-filetype=obj", path, "-o", directory / "blocks.o"])
    rtl.run(["ld.lld", "-m", "elf32lriscv", "-Ttext=0x2000", directory / "blocks.o", "-o", directory / "blocks.elf"])
    (directory / "blocks.objdump").write_text(rtl.run([
        "llvm-objdump", "-D", "--section=.text", "--mattr=+m,+zba", directory / "blocks.elf"]))
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before-emitter", type=Path, required=True)
    parser.add_argument("--before-rtl", type=Path, required=True)
    parser.add_argument("--rtl-out", type=Path, required=True)
    parser.add_argument("--out", type=Path, default=rtl.HERE / "out/jit-isa/cost")
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    rtl.OUT = out
    build = rtl.run(["cargo", "build", "--release", "--no-default-features", "--features", "r86chip",
                     "--lib", "--message-format=json"], rtl.ROOT)
    artifacts = [json.loads(line) for line in build.splitlines() if line.startswith('{')]
    library = next(Path(name) for artifact in artifacts if artifact.get("reason") == "compiler-artifact"
                   and artifact["target"]["name"] == "r86" for name in artifact["filenames"] if name.endswith('.rlib'))
    results, inputs = {}, {}
    for label, emitter, rtl_out in (("before", args.before_emitter, args.before_rtl),
                                    ("after", rtl.HERE.parent / "jit/compile.rs", args.rtl_out)):
        emitter = emitter.resolve()
        directory = out / label
        directory.mkdir(exist_ok=True)
        executable = rtl_out.resolve() / "b2/obj/Vcore_tb"
        inputs[label] = dict(emitter_sha256=hashlib.sha256(emitter.read_bytes()).hexdigest(),
                             rtl_sha256=hashlib.sha256(executable.read_bytes()).hexdigest())
        results[label] = measure(emitted(emitter, directory, library), directory, executable)
    for before, after in zip(results['before'], results['after'], strict=True):
        assert (before['case'], before['count'], before['frame']) == (after['case'], after['count'], after['frame'])
    marginal = {}
    for label, rows in results.items():
        grouped = {row['case']: {} for row in rows}
        for row in rows:
            grouped[row['case']][row['count']] = row
        marginal[label] = {name: {key: (pair[16][key] - pair[1][key]) / 15
                                  for key in ('emitted_words', 'retired_native', 'cycles')}
                           for name, pair in grouped.items() if 16 in pair}
    report = dict(scope=__doc__, inputs=inputs, results=results, marginal=marginal,
                  iss_sha256=hashlib.sha256(rtl.SIMULATOR.read_bytes()).hexdigest())
    (out / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(marginal, indent=2))


if __name__ == '__main__':
    main()
