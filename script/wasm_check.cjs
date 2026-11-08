#!/usr/bin/env node
"use strict";
/** A real x86 PIO guest checks the async boundary, not a mocked CPU.
 * The guest writes sectors across 2 MiB, FLUSHes, and a fresh instance verifies
 * both data and untouched neighbours. A second-sector failure must leave only
 * the first sector committed, report ATA failure, and never replay either OUT.
 * Uses the existing native disk fixture; no guest-specific production path.
 */
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const os = require("node:os");
const path = require("node:path");
const {pathToFileURL} = require("node:url");
const {setImmediate: yieldTurn} = require("node:timers/promises");
const root = path.resolve(__dirname, "..");

async function main() {
    const {WasmPc} = await import(pathToFileURL(path.join(root, "web/wasm.mjs")).href);
    const wasm = await fs.readFile(process.argv[2] ?? path.join(root, "web/dist/r86-asyncify.wasm"));
    const guest = await fs.readFile(path.join(root, "tests/bootsectors/vhd_pio.bin"));
    const dir = await fs.mkdtemp(path.join(os.tmpdir(), "r86-wasm-pio-"));
    try {
        const disk = path.join(dir, "disk.raw");
        const prepare = async () => {
            const f = await fs.open(disk, "w+");
            await f.truncate(8 * 1024 ** 2);
            await f.close();
        };
        async function boot(failSecondWrite = false) {
            const file = await fs.open(disk, "r+");
            const writes = [], errors = [];
            let flushes = 0, refusedReentry = false, pc;
            const io = {
                async read(id, offset, length) {
                    assert.equal(id, 0);
                    // Keep a real pending interval and attempt the kind of
                    // reentry a queued input event could otherwise cause.
                    await yieldTurn();
                    if (!refusedReentry) {
                        assert.throws(() => pc.steps(), /reentry/);
                        refusedReentry = true;
                    }
                    const bytes = Buffer.alloc(length);
                    const {bytesRead} = await file.read(bytes, 0, length, offset);
                    assert.equal(bytesRead, length);
                    return bytes;
                },
                async write(id, offset, bytes) {
                    assert.equal(id, 0);
                    writes.push(offset);
                    await yieldTurn();
                    if (failSecondWrite && writes.length === 2) throw new Error("injected media failure");
                    const {bytesWritten} = await file.write(bytes, 0, bytes.length, offset);
                    assert.equal(bytesWritten, bytes.length);
                },
                async flush() { flushes++; await file.sync(); },
            };
            try {
                pc = new WasmPc(io, {onIoError: error => errors.push(error)});
                await pc.init(wasm, {memoryMiB: 16, diskSectors: 8 * 1024 ** 2 / 512, deterministic: true, readOnly: false});
                assert.ok(pc.asyncData, "test needs the actual Asyncify build");
                await pc.loadRom(2, guest);
                while (pc.steps() < 1_000_000 && !pc.serial().length) {
                    assert.ok(await pc.run(10000, true) >= 0, pc.text(3));
                }
                assert.ok(refusedReentry && pc.suspensions > 0);
                return {serial: Buffer.from(pc.serial()).toString(), writes, errors, flushes};
            } finally { await file.close(); }
        }
        await prepare();
        const written = await boot();
        assert.equal(written.serial, "W");
        assert.deepEqual(written.writes, [4095 * 512, 4096 * 512]);
        assert.ok(written.flushes > 0);
        assert.deepEqual(written.errors, []);
        const reopened = await boot();
        assert.equal(reopened.serial, "R");
        assert.deepEqual(reopened.writes, []);
        await prepare();
        const failed = await boot(true);
        assert.equal(failed.serial, "E");
        assert.deepEqual(failed.writes, written.writes);
        assert.equal(failed.errors.length, 1);
        const file = await fs.open(disk, "r");
        const sectors = Buffer.alloc(1024);
        try { await file.read(sectors, 0, sectors.length, 4095 * 512); }
        finally { await file.close(); }
        for (let i = 0; i < 512; i += 2) assert.equal(sectors.readUInt16LE(i), 0x6a3c);
        assert.ok(sectors.subarray(512).every(byte => byte === 0));
        console.log("Wasm PIO: committed writes, reopen, async reentry guard and partial-failure boundary passed");
    } finally { await fs.rm(dir, {recursive: true, force: true}); }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
