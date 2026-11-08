"use strict";
/** Executes the framebuffer lifetime guest with both engines. Guest assertions
 * cover PCI/PTE changes; host pixel checks observe dirty-page notification after
 * rendering clears it. Usage: node script/wasm_jit_lfb.cjs [core.wasm].
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
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "r86-wasm-lfb-"));
    try {
        const rom = path.join(dir, "guest.bin");
        execFileSync("nasm", ["-f", "bin", path.join(root, "tests/roms/wasm_jit_lfb.asm"), "-o", rom]);
        const {WasmPc} = await import(pathToFileURL(path.join(root, "web/wasm.mjs")).href);
        const wasm = fs.readFileSync(process.argv[2] ?? path.join(root, "web/dist/r86-jit.wasm"));
        for (const engine of ["interp", "jit"]) {
            const pc = new WasmPc({read() { throw new Error("guest has no disk"); }});
            await pc.init(wasm, {memoryMiB: 16, engine, jitMiB: 40, jitThreshold: 64, deterministic: true});
            await pc.loadRom(0, fs.readFileSync(rom));
            const start = performance.now();
            let observed = "";
            while (!observed.endsWith("K")) {
                assert.ok(performance.now() - start < 30000, `${engine}: guest timed out`);
                const status = await pc.run(10000, true);
                assert.ok(status >= 0, pc.text(3));
                const serial = Buffer.from(pc.serial()).toString("latin1");
                assert.ok(!serial.includes("F"), `${engine}: guest assertions failed: ${serial}\n${pc.text(0)}`);
                if (serial !== observed) {
                    assert.equal(serial.length, observed.length + 1, "stop at each serial boundary");
                    const colors = {R: [255, 0, 0, 255], G: [0, 255, 0, 255], B: [0, 0, 255, 255]};
                    const color = colors[serial.at(-1)];
                    if (color) {
                        const frame = pc.frame();
                        assert.equal(frame.width, 800);
                        assert.equal(frame.height, 600);
                        assert.deepEqual([...frame.pixels.slice(0, 4)], color, `${engine}: ${serial}`);
                    }
                    observed = serial;
                }
                if (!observed.endsWith("K")) assert.notEqual(status, 1, "unexpected halt");
                await immediate();
            }
            assert.equal(observed, "RGBK");
            if (engine === "jit") {
                const report = pc.text(4);
                const count = key => Number(new RegExp(`${key}: (\\d+)`).exec(report)?.[1] ?? 0);
                assert.ok(count("native_steps") > 3600000, "framebuffer loops must actually execute generated code\n" + report);
            }
            console.log(`${engine}: framebuffer bytes, dirty rendering, PCI decode and read-only paging OK`);
        }
    } finally { fs.rmSync(dir, {recursive: true, force: true}); }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
