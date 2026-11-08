"use strict";
// Invoked by the Rust differential test; executes actual emitted modules in
// V8. RAM TLB hits and denied/cross-page exits use the same generated guards.
const fs = require("node:fs");
const {abi: a, programs, cases} = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const memory = new WebAssembly.Memory({initial: 16});
const table = new WebAssembly.Table({initial: 2 * a.group_size, element: "anyfunc"});
const bytes = new Uint8Array(memory.buffer), words = new DataView(memory.buffer);
const frame = 0x1000, ram = 0x20000, tlb = 0x40000, entries = 0x50000, links = 0x60000;
const store = (at, value) => words.setUint32(at, value >>> 0, true);
const load = at => words.getUint32(at, true);
const compiled = programs.map(p => p.regions.map(r => new WebAssembly.Instance(new WebAssembly.Module(Uint8Array.from(r.module)),
    {r: {memory, table}}).exports));
for (const [index, regs, [flags, lhs, value, kind], budget, permissions, [state, readProof, writeProof]] of cases) {
    const p = programs[index];
    bytes.fill(0);
    for (let at = 0x2200; at < 0x2600; at++) bytes[ram + at] = (at * 37 ^ at >>> 4) & 255;
    bytes.set(p.code, ram); bytes[ram + p.code.length] = 0xcc;
    const before = bytes.slice(ram, ram + 65536);
    // GPRs/flags/budget are arguments, not record inputs. Poison old outputs
    // so an incomplete return cannot pass by retaining the invocation's input.
    regs.forEach((_, i) => store(frame + a.regs + i * 4, 0xdeadbeef));
    for (const key of ["eip", "eflags", "remaining", "exit", "flag_lhs", "flag_result", "flag_kind"]) store(frame + a[key], 0xdeadbeef);
    for (const [key, value] of Object.entries({tlb, tlb_state: state,
        permissions, popf_mask: 0x257fd5})) store(frame + a[key], value);
    for (let page = 0; page < 16; page++) {
        const row = tlb + page * a.tlb_size;
        // Match the Rust RAM fixture's alias: guest page 7 maps to page 2.
        store(row + a.tlb_bias, ram - (page === 7 ? 0x5000 : 0)); store(row + a.tlb_bias + 4, 0);
        store(row + a.tlb_tag, (page << 12) | readProof);
        store(row + a.tlb_write_tag, writeProof ? (page << 12) | writeProof : 0);
    }
    p.regions.forEach((r, page) => {
        const at = entries + page * 16384, row = links + page * a.link_size;
        for (const [pc, id] of r.entries) words.setUint32(at + pc * 4, ((page * a.group_size + 1) << a.entry_bits) + (id - 1), true);
        for (let i = 0; i < a.group_size; i++) table.set(page * a.group_size + i, compiled[index][page][i ? `run${i}` : "run"] ?? null);
        store(row + a.link_tag, page * 4096 | 1); store(row + a.link_function, 0);
        store(row + a.link_entries, at); store(row + a.link_version_ptr, 0x70000);
        store(row + a.link_version, 1); store(0x70000, 1);
    });
    const start = p.regions[0].entry, f = start >> a.entry_bits;
    compiled[index][0][f ? `run${f}` : "run"](start & ((1 << a.entry_bits) - 1),
        ...regs, flags, budget, lhs, value, kind);
    const result = regs.map((_, i) => load(frame + a.regs + i * 4));
    for (const key of ["eip", "eflags", "remaining", "exit", "repeat_control", "repeat_completed", "repeat_next", "flag_lhs", "flag_result", "flag_kind"]) result.push(load(frame + a[key]));
    for (let at = 0; at < before.length; at++) if (before[at] !== bytes[ram + at]) result.push(at, bytes[ram + at]);
    process.stdout.write(result.join(" ") + "\n");
}
