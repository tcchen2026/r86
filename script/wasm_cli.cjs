#!/usr/bin/env node
"use strict";
/** Run the shared Wasm machine under Node, using interpretation or the optional
 * x86-to-Wasm JIT. Default ISO I/O uses an immutable buffer like the native frontend.
 * --async-io uses asynchronous positional file reads and Asyncify instead.
 * No guest-code cache, snapshot, or pre-executed boot state is loaded/saved.
 * Raw HDDs are read-only in this diagnostic CLI; browser writes belong to the
 * IndexedDB backend. --report records process CPU time and the exact endpoint.
 * --jit-trace writes bounded diagnostic JSONL from a --profile build. It is
 * never an execution input, and instrumented elapsed time is not a benchmark.
 */
const fs = require("node:fs");
const path = require("node:path");
const {parseArgs} = require("node:util");
const {pathToFileURL} = require("node:url");
const {performance} = require("node:perf_hooks");
const {setImmediate: immediate, setTimeout: delay} = require("node:timers/promises");
const root = path.resolve(__dirname, "..");

const HELP = `Usage: node script/wasm_cli.cjs [options]
  --wasm FILE              Wasm module (default: web/dist/r86.wasm)
  --iso FILE               Bootable ISO, read-only; no implicit rewriting
  --hdd FILE               Raw HDD, read-only (VHD is not supported here)
  --bios FILE              Default: firmware/seabios/bios.bin
  --vga-bios FILE           Default: firmware/seabios/vgabios-stdvga.bin
  --bootsector FILE         Run a 512-byte guest fixture without BIOS entry
  --boot-order cdrom|disk   Default: cdrom
  --firmware-display serial|vga  Default: serial
  --mem-mib N              Guest RAM, 16..512 (default: 128)
  --clock realtime|deterministic (default: realtime)
  --cpu-engine interp|jit  Guest execution engine (default: interp)
  --jit-cache-mib N        Charged JIT cache budget (default: 128, RAM is separate)
  --jit-threshold N        Hot-entry observation threshold (default: 256)
  --jit-dump DIR           Diagnostic output of generated modules; never loaded
  --jit-trace FILE.jsonl   Lifecycle/sampling trace; requires --jit --profile build
  --steps N                Instruction limit (default: 10000000000)
  --timeout N              Wall time limit in seconds (default: 600)
  --slice-steps N          Host yield interval (default: 100000)
  --expect-serial TEXT     Successful endpoint; exit nonzero if never reached
  --expect-frame FILE.rgb  Match x y r g b rows, including the requested size
  --frame-size WxH         Expected dimensions (default: 800x600)
  --frame-ms N             Frame observation interval (default: 100)
  --async-io               Real async file I/O; requires an Asyncify module
  --frame FILE.ppm         Save final VGA frame
  --report FILE.json       Timing, I/O counters, stop reason and CPU state
  --quiet                  Suppress guest serial output (report still includes it)
  --progress               Print guest steps and JIT totals every ten seconds
  --help
`;

function options() {
    const strings = ["wasm", "iso", "hdd", "bios", "vga-bios", "bootsector", "boot-order",
        "firmware-display", "mem-mib", "clock", "steps", "timeout", "slice-steps", "expect-serial", "frame", "report",
        "cpu-engine", "jit-cache-mib", "jit-threshold", "jit-dump", "jit-trace", "expect-frame", "frame-size", "frame-ms"];
    const opts = Object.fromEntries(strings.map(key => [key, {type: "string"}]));
    for (const key of ["async-io", "quiet", "progress", "help"]) opts[key] = {type: "boolean"};
    const {values: a} = parseArgs({options: opts});
    if (a.help) { process.stdout.write(HELP); return null; }
    function integer(key, fallback, min, max) {
        const n = Number(a[key] ?? fallback);
        if (!Number.isSafeInteger(n) || n < min || n > max) throw new Error(`Invalid --${key}`);
        return n;
    }
    a.mem = integer("mem-mib", 128, 16, 512);
    a.limit = integer("steps", 10000000000, 0, Number.MAX_SAFE_INTEGER);
    a.timeout = integer("timeout", 600, 1, 86400);
    a.slice = integer("slice-steps", 100000, 1, 0xffffffff);
    a.jitMiB = integer("jit-cache-mib", 128, 40, 1024);
    a.jitThreshold = integer("jit-threshold", 256, 1, 65534);
    a.frameMs = integer("frame-ms", 100, 1, 60000);
    a.engine = a["cpu-engine"] ?? "interp";
    if (!["interp", "jit"].includes(a.engine)) throw new Error("Invalid --cpu-engine");
    if (a["jit-trace"] && a.engine !== "jit") throw new Error("--jit-trace requires --cpu-engine jit");
    a.frameSize = (a["frame-size"] ?? "800x600").split("x").map(Number);
    if (a.frameSize.length !== 2 || !a.frameSize.every(n => Number.isInteger(n) && n > 0 && n <= 4096)) throw new Error("Invalid --frame-size");
    a.clock ??= "realtime";
    a["boot-order"] ??= "cdrom";
    a["firmware-display"] ??= "serial";
    if (!["realtime", "deterministic"].includes(a.clock) ||
        !["cdrom", "disk"].includes(a["boot-order"]) ||
        !["serial", "vga"].includes(a["firmware-display"])) throw new Error("Invalid clock, boot order or display");
    if (a["expect-serial"] === "") throw new Error("Empty serial expectation");
    a.wasm ??= path.join(root, a.engine === "jit" ? "web/dist/r86-jit.wasm" : "web/dist/r86.wasm");
    a.bios ??= path.join(root, "firmware/seabios/bios.bin");
    a["vga-bios"] ??= path.join(root, "firmware/seabios/vgabios-stdvga.bin");
    return a;
}

function saveFrame(filename, frame) {
    if (!frame) return;
    const rgb = Buffer.allocUnsafe(frame.width * frame.height * 3);
    for (let i = 0, j = 0; i < frame.pixels.length; i += 4, j += 3) {
        rgb[j] = frame.pixels[i]; rgb[j + 1] = frame.pixels[i + 1]; rgb[j + 2] = frame.pixels[i + 2];
    }
    fs.writeFileSync(filename, Buffer.concat([Buffer.from(`P6\n${frame.width} ${frame.height}\n255\n`), rgb]));
}

async function main() {
    const a = options();
    if (!a) return;
    const started = performance.now(), cpuStarted = process.cpuUsage();
    const descriptors = [], files = [];
    const ioStats = {reads: 0, read_bytes: 0, writes: 0, flushes: 0, errors: []};
    let pc, reason = "error", serialSeen = 0, matched = false, serialTail = "";
    let initializationSeconds, endpointSeconds, endpointCpu;
    let lastFrame, polls = 0;
    let traceFd, traceBytes = 0, traceTruncated = false, traceReady = false, traceError, nextTrace = 0;
    function drainTrace() {
        if (traceFd === undefined || !pc?.handle || pc.poisoned || pc.busy) return;
        const trace = pc.text(5);
        if (!trace.startsWith('{"schema":1,')) throw new Error("--jit-trace requires a Wasm core built with --jit --profile");
        if (traceTruncated) return;
        // Keep u64 keys as exact JSON integers; do not parse/re-encode in JS.
        const line = `{"elapsed_seconds":${(performance.now() - started) / 1000},"trace":${trace}}\n`;
        const size = Buffer.byteLength(line);
        if (traceBytes + size > 128 * 1024 ** 2) {
            traceTruncated = true;
            fs.writeSync(traceFd, '{"truncated":true,"reason":"128 MiB output limit"}\n');
            return;
        }
        fs.writeSync(traceFd, line);
        traceBytes += size;
    }
    const points = a["expect-frame"] ? fs.readFileSync(a["expect-frame"], "utf8").split(/\r?\n/)
        .map(line => line.trim()).filter(line => line && !line.startsWith("#")).map(line => {
            const row = line.split(/\s+/).map(Number);
            if (row.length !== 5 || !row.every(Number.isInteger) || row[0] < 0 || row[0] >= a.frameSize[0] ||
                row[1] < 0 || row[1] >= a.frameSize[1] || !row.slice(2).every(n => n >= 0 && n <= 255)) throw new Error("Invalid frame signature");
            return row;
        }) : null;
    if (points?.length === 0) throw new Error("Empty frame signature");
    if (a["jit-dump"]) fs.mkdirSync(a["jit-dump"], {recursive: true});
    const marker = a["expect-serial"];
    let interrupted = false;
    const interrupt = () => { interrupted = true; };
    process.on("SIGINT", interrupt);
    process.on("SIGTERM", interrupt);
    try {
        for (const [id, filename] of [[0, a.hdd], [1, a.iso]]) {
            if (!filename) continue;
            const fd = fs.openSync(filename, "r");
            descriptors.push(fd);
            const size = fs.fstatSync(fd).size;
            if (!Number.isSafeInteger(size) || size === 0 || size % (id ? 2048 : 512) !== 0 ||
                size > (id ? 0xffffffff : 128 * 1024 ** 3)) throw new Error(`Invalid image size: ${filename}`);
            if (!id && /\.(vhd|vhdx|qcow2|vmdk)$/i.test(filename)) throw new Error("--hdd currently accepts raw images only");
            // Buffer ISO bytes once just as r86-run does, unless exercising
            // actual async I/O. HDDs remain positional reads from the file.
            files[id] = {fd, size, bytes: id && !a["async-io"] ? fs.readFileSync(fd) : null};
        }
        const io = {
            read(id, offset, length) {
                ioStats.reads++; ioStats.read_bytes += length;
                const file = files[id];
                if (!file || !Number.isSafeInteger(offset) || offset < 0 || offset + length > file.size) {
                    throw new Error("Disk read outside attached image");
                }
                if (file.bytes) return file.bytes.subarray(offset, offset + length);
                const buffer = Buffer.allocUnsafe(length);
                if (a["async-io"]) return new Promise((resolve, reject) => {
                    fs.read(file.fd, buffer, 0, length, offset, (error, count) => {
                        if (error) reject(error);
                        else if (count !== length) reject(new Error("Short disk read"));
                        else resolve(buffer);
                    });
                });
                if (fs.readSync(file.fd, buffer, 0, length, offset) !== length) throw new Error("Short disk read");
                return buffer;
            },
            write() { ioStats.writes++; throw new Error("CLI disk images are read-only"); },
            flush() { ioStats.flushes++; },
        };
        const {WasmPc} = await import(pathToFileURL(path.join(root, "web/wasm.mjs")).href);
        pc = new WasmPc(io, {
            onLog: message => process.stderr.write(`wasm: ${message}\n`),
            onIoError: error => { ioStats.errors.push(error); process.stderr.write(`disk: ${JSON.stringify(error)}\n`); },
            onModule: a["jit-dump"] ? (bytes, {request}) => fs.writeFileSync(path.join(a["jit-dump"], `${request}.wasm`), bytes) : undefined,
        });
        await pc.init(fs.readFileSync(a.wasm), {
            memoryMiB: a.mem, diskSectors: (files[0]?.size ?? 0) / 512, cdBytes: files[1]?.size ?? 0,
            cdFirst: a["boot-order"] === "cdrom", serial: a["firmware-display"] === "serial",
            deterministic: a.clock === "deterministic", readOnly: true,
            engine: a.engine, jitMiB: a.jitMiB, jitThreshold: a.jitThreshold,
        });
        if (a["jit-trace"]) {
            traceFd = fs.openSync(a["jit-trace"], "w");
            descriptors.push(traceFd);
            drainTrace();
            traceReady = true;
        }
        if (a["async-io"] && !pc.asyncData) throw new Error("--async-io requires a module built with --asyncify");
        if (a.bootsector) await pc.loadRom(2, fs.readFileSync(a.bootsector));
        else {
            await pc.loadRom(0, fs.readFileSync(a.bios));
            await pc.loadRom(1, fs.readFileSync(a["vga-bios"]));
        }
        initializationSeconds = (performance.now() - started) / 1000;
        let lastYield = performance.now();
        let nextFrame = performance.now() + a.frameMs;
        let nextProgress = performance.now() + 10000;
        reason = "step-limit";
        while (pc.steps() < a.limit) {
            if (interrupted) { reason = "interrupted"; break; }
            if (performance.now() - started > a.timeout * 1000) { reason = "timeout"; break; }
            const status = await pc.run(Math.min(a.slice, a.limit - pc.steps()), Boolean(marker));
            if (traceFd !== undefined && performance.now() >= nextTrace) {
                drainTrace();
                nextTrace = performance.now() + 250;
            }
            if (a.progress && performance.now() >= nextProgress) {
                nextProgress = performance.now() + 10000;
                process.stderr.write(`R86_PROGRESS elapsed=${((performance.now() - started) / 1000).toFixed(3)} steps=${pc.steps()} ${pc.text(4).split("\n")[0]}\n`);
            }
            const bytes = pc.serial(serialSeen);
            serialSeen += bytes.length;
            if (bytes.length) {
                const text = Buffer.from(bytes).toString("latin1");
                if (!a.quiet) process.stdout.write(bytes);
                if (marker) {
                    serialTail += text;
                    if (serialTail.includes(marker)) { matched = true; reason = "serial-marker"; break; }
                    serialTail = marker.length > 1 ? serialTail.slice(-(marker.length - 1)) : "";
                }
            }
            if (status < 0) { reason = "guest-stop"; throw new Error(pc.text(3)); }
            if (status === 2) { reason = "poweroff"; break; }
            if (points && performance.now() >= nextFrame) {
                nextFrame = performance.now() + a.frameMs;
                lastFrame = pc.frame() ?? lastFrame;
                polls++;
                if (lastFrame && lastFrame.width === a.frameSize[0] && lastFrame.height === a.frameSize[1] &&
                    points.every(([x, y, r, g, b]) => {
                        const i = (y * lastFrame.width + x) * 4;
                        return lastFrame.pixels[i] === r && lastFrame.pixels[i + 1] === g && lastFrame.pixels[i + 2] === b;
                    })) {
                    matched = true; reason = "frame-marker";
                    process.stdout.write(`R86_WASM_FRAME elapsed=${((performance.now() - started) / 1000).toFixed(6)} steps=${pc.steps()} polls=${polls}\n`);
                    break;
                }
            }
            if (status === 1) {
                if (a.clock === "deterministic") { reason = "halted"; break; }
                await delay(1);
            } else if (performance.now() - lastYield >= (pc.jit.pending ? 1 : 20)) {
                await immediate(); // signals and async I/O can make progress
                lastYield = performance.now();
            }
        }
        endpointSeconds = (performance.now() - started) / 1000;
        endpointCpu = process.cpuUsage(cpuStarted);
        await pc.control(2);
        if (a.frame) saveFrame(a.frame, pc.frame() ?? lastFrame);
        if (marker && !matched) throw new Error(`Serial marker not reached (${reason}): ${marker}`);
        if (points && !matched) throw new Error(`Frame marker not reached (${reason})`);
        if (reason === "timeout" || reason === "interrupted") process.exitCode = 1;
    } catch (error) {
        process.exitCode = 1;
        process.stderr.write(`wasm_cli: ${error.stack ?? error}\n`);
        if (pc?.handle && !pc.poisoned && !pc.busy) process.stderr.write(pc.text(2) + "\n");
    } finally {
        endpointSeconds ??= (performance.now() - started) / 1000;
        endpointCpu ??= process.cpuUsage(cpuStarted);
        if (traceReady) {
            try {
                drainTrace();
                fs.writeSync(traceFd, JSON.stringify({end: true, elapsed_seconds: endpointSeconds,
                    stop: reason, matched, truncated: traceTruncated, poisoned: Boolean(pc?.poisoned)}) + "\n");
            } catch (error) {
                traceError = String(error);
                process.exitCode = 1;
                process.stderr.write(`trace: ${traceError}\n`);
            }
        }
        const report = {
            engine: a.engine === "jit" ? "x86-to-wasm-jit" : "shared-rust-interpreter-wasm", node: process.version, v8: process.versions.v8,
            node_flags: process.execArgv, jit_cache_mib: a.jitMiB, jit_threshold: a.jitThreshold,
            jit: pc?.jit.stats, jit_core: pc?.handle && !pc.poisoned ? pc.text(4) : "",
            jit_trace: traceFd === undefined ? undefined : {file: a["jit-trace"], bytes: traceBytes, truncated: traceTruncated, error: traceError},
            peak_rss_kib: process.resourceUsage().maxRSS, polls,
            memory: process.memoryUsage(), wasm_memory_bytes: pc?.exports?.memory.buffer.byteLength,
            wasm: path.resolve(a.wasm), clock: a.clock, async_io: Boolean(a["async-io"]),
            initialization_seconds: initializationSeconds, elapsed_seconds: endpointSeconds,
            cpu_seconds: (endpointCpu.user + endpointCpu.system) / 1e6,
            steps: pc?.handle && !pc.poisoned ? pc.steps() : null, stop: reason, matched,
            suspensions: pc?.suspensions ?? 0, io: ioStats,
            serial: pc?.handle && !pc.poisoned ? Buffer.from(pc.serial()).toString("latin1").slice(-65536) : "",
            diagnostics: pc?.handle && !pc.poisoned ? pc.text(2) : "Wasm trap; instance cannot be reentered",
        };
        if (a.report) fs.writeFileSync(a.report, JSON.stringify(report, null, 2) + "\n");
        const {serial, diagnostics, ...summary} = report;
        process.stderr.write(`R86_WASM_STOP ${JSON.stringify(summary)}\n`);
        for (const fd of descriptors) fs.closeSync(fd);
        process.off("SIGINT", interrupt); process.off("SIGTERM", interrupt);
    }
}

if (require.main === module) {
    // V8's background Wasm compilation alone does not keep Node's event loop
    // alive. This also covers eager optimizing compilation during core startup.
    const lifetime = setInterval(() => {}, 1000);
    main().catch(error => { console.error(error); process.exitCode = 1; })
        .finally(() => clearInterval(lifetime));
}
module.exports = {main};
