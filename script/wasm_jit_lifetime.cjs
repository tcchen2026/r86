"use strict";
/** Real guest SMC, descriptor reload/A-bit faults, paging and eviction/reentry.
 * The test delays host publication, not x86 writes or V8 compilation. The
 * 40-MiB JIT budget deliberately leaves little room after compiler scratch.
 * Usage: node script/wasm_jit_lifetime.cjs [core.wasm] [assembled-guest.bin].
 * The optional ROM supports ARM64 targets after assembling on the build host.
 */
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {execFileSync} = require("node:child_process");
const {pathToFileURL} = require("node:url");
const {setImmediate: immediate} = require("node:timers/promises");
const root = path.resolve(__dirname, "..");

async function main() {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "r86-wasm-lifetime-"));
    try {
        const bios = process.argv[3] ?? path.join(dir, "guest.bin");
        if (!process.argv[3]) execFileSync("nasm", ["-f", "bin", path.join(root, "tests/roms/wasm_jit_lifetime.asm"), "-o", bios]);
        const {WasmPc} = await import(pathToFileURL(path.join(root, "web/wasm.mjs")).href);
        const pc = new WasmPc({read() { throw new Error("guest has no disk"); }});
        await pc.init(fs.readFileSync(process.argv[2] ?? path.join(root, "web/dist/r86-jit.wasm")),
            {memoryMiB: 16, engine: "jit", jitMiB: 40, jitThreshold: 64, deterministic: true});
        await pc.loadRom(0, fs.readFileSync(bios));
        let publish = false;
        const publishReady = pc.jit.publish.bind(pc.jit);
        pc.jit.publish = () => { if (publish) publishReady(); };
        const start = performance.now();
        for (;;) {
            assert.ok(performance.now() - start < 30000, "guest lifetime check timed out");
            const status = await pc.run(10000);
            assert.ok(status >= 0, pc.text(3));
            const serial = Buffer.from(pc.serial()).toString("latin1");
            assert.ok(!serial.includes("F"), "guest code-cache lifetime or execute-permission check failed");
            if (serial.includes("W")) {
                if (!publish) assert.ok(pc.jit.pending, "the guest must write while compilation is pending");
                publish = true;
            }
            if (serial.includes("K")) break;
            assert.notEqual(status, 1, "unexpected guest halt");
            if (publish) await immediate();
        }
        const report = pc.text(4);
        const count = key => Number(new RegExp(`${key}: (\\d+)`).exec(report)?.[1] ?? 0);
        assert.ok(count("native_steps") > 100000, report);
        assert.ok(count("cached_segment_loads") > 100, "warmed segment loads must use the shared descriptor fast path\n" + report);
        assert.ok(count("discarded") > 0, "changed code must reject the delayed compilation\n" + report);
        assert.ok(count("invalidated") > 0, "the published instruction write must retire generated dependencies\n" + report);
        assert.ok(count("evicted") > 0, "the real guest must exceed the resident cache");
        assert.ok(count("peak_charged_bytes") <= 40 * 1024 ** 2, report);
        console.log("Wasm SMC/segments/paging/privilege/eviction guest: OK\n" + report);
    } finally { fs.rmSync(dir, {recursive: true, force: true}); }
}
const lifetime = setInterval(() => {}, 1000);
main().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => clearInterval(lifetime));
