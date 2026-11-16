import {WasmPc} from "./wasm.mjs";

// A fresh Worker owns one VM and this bounded LRU cache. No media is written.
class RangeDisk {
    constructor(bytes, chunkBytes, capBytes) {
        this.bytes = bytes;
        this.chunkBytes = chunkBytes;
        this.capBytes = capBytes;
        this.cache = new Map();
        this.stats = {calls: 0, bytes: 0, readMs: 0, requests: 0, fetchedBytes: 0,
            xhrMs: 0, hits: 0, cacheBytes: 0, peakCacheBytes: 0};
    }
    chunk(start) {
        let bytes = this.cache.get(start);
        if (bytes) {
            this.stats.hits++;
            this.cache.delete(start);
            this.cache.set(start, bytes);
            return bytes;
        }
        const end = Math.min(start + this.chunkBytes, this.bytes) - 1;
        const request = new XMLHttpRequest();
        request.open("GET", "/media.iso", false);
        request.responseType = "arraybuffer";
        request.setRequestHeader("Range", `bytes=${start}-${end}`);
        const before = performance.now();
        this.stats.requests++;
        try { request.send(); }
        finally { this.stats.xhrMs += performance.now() - before; }
        if (request.status !== 206 || request.getResponseHeader("Content-Range") !== `bytes ${start}-${end}/${this.bytes}` ||
            !(request.response instanceof ArrayBuffer) || request.response.byteLength !== end - start + 1) {
            throw new Error(`Invalid media range response: HTTP ${request.status} for ${start}-${end}`);
        }
        bytes = new Uint8Array(request.response);
        this.stats.fetchedBytes += bytes.length;
        while (this.stats.cacheBytes + bytes.length > this.capBytes) {
            const key = this.cache.keys().next().value;
            this.stats.cacheBytes -= this.cache.get(key).length;
            this.cache.delete(key);
        }
        this.cache.set(start, bytes);
        this.stats.cacheBytes += bytes.length;
        this.stats.peakCacheBytes = Math.max(this.stats.peakCacheBytes, this.stats.cacheBytes);
        return bytes;
    }
    read(drive, offset, length) {
        const before = performance.now();
        this.stats.calls++;
        this.stats.bytes += length;
        try {
            if (drive !== 1 || !Number.isSafeInteger(offset) || !Number.isSafeInteger(length) ||
                offset < 0 || length < 1 || offset + length > this.bytes) throw new Error("Read outside attached ISO");
            const result = new Uint8Array(length);
            for (let copied = 0; copied < length;) {
                const position = offset + copied;
                const start = Math.floor(position / this.chunkBytes) * this.chunkBytes;
                const chunk = this.chunk(start);
                const count = Math.min(length - copied, chunk.length - (position - start));
                result.set(chunk.subarray(position - start, position - start + count), copied);
                copied += count;
            }
            return result;
        } finally { this.stats.readMs += performance.now() - before; }
    }
    write() { throw new Error("Benchmark media is read-only"); }
    flush() {}
}

async function get(url, text = false) {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
    return text ? response.text() : new Uint8Array(await response.arrayBuffer());
}

function signature(text) {
    const points = text.split(/\r?\n/).map(line => line.trim()).filter(line => line && !line.startsWith("#"))
        .map(line => line.split(/\s+/).map(Number));
    if (!points.length || points.some(row => row.length !== 5 || !row.every(Number.isInteger) ||
        row[0] < 0 || row[0] >= 800 || row[1] < 0 || row[1] >= 600 || row.slice(2).some(n => n < 0 || n > 255))) {
        throw new Error("Invalid language-dialog frame signature");
    }
    return points;
}

async function run(options) {
    const channel = new MessageChannel();
    let resume;
    channel.port1.onmessage = () => resume();
    const yieldTurn = () => new Promise(resolve => { resume = resolve; channel.port2.postMessage(null); });
    const elapsed = () => performance.timeOrigin + performance.now() - options.startedAt;
    let pc, io, frame, ready = false, steps = 0, initMs = null, polls = 0;
    let runMs = 0, slices = 0, haltedSlices = 0;
    let matched = false, stop = "error", error = null, diagnostics = "", jitCore = "";
    const logs = [], ioErrors = [];
    const memoryBytes = () => pc?.exports?.memory.buffer.byteLength ?? 0;
    const sample = () => {
        const wallMs = elapsed(), readMs = io?.stats.readMs ?? 0;
        return {engine: options.engine, steps, wallMs, readMs, runMs, slices, haltedSlices,
            mips: wallMs > readMs ? steps / ((wallMs - readMs) * 1000) : null,
            busyMips: runMs > readMs ? steps / ((runMs - readMs) * 1000) : null,
            peakMemoryBufferBytes: memoryBytes(), jit: pc?.jit.stats ?? null};
    };
    try {
        if (!["interp", "jit"].includes(options.engine)) throw new Error("Invalid engine");
        for (const [key, min, max] of [["memoryMiB", 16, 512], ["cdBytes", 2048, 0xffffffff],
            ["chunkBytes", 2048, 4 * 1024 ** 2], ["cacheBytes", options.chunkBytes, 128 * 1024 ** 2],
            ["stepCap", 1, Number.MAX_SAFE_INTEGER], ["sliceSteps", 1, 0xffffffff],
            ["timeoutMs", 1, 86400000], ["frameMs", 1, 60000]]) {
            if (!Number.isSafeInteger(options[key]) || options[key] < min || options[key] > max) throw new Error(`Invalid ${key}`);
        }
        if (options.clock !== "realtime") throw new Error("This benchmark uses realtime clocks");
        io = new RangeDisk(options.cdBytes, options.chunkBytes, options.cacheBytes);
        const [core, bios, vga, expected] = await Promise.all([
            get("/dist/r86-jit.wasm"), get("/bios.bin"), get("/vgabios-stdvga.bin"), get("/langsel.rgb", true),
        ]);
        const points = signature(expected);
        pc = new WasmPc(io, {
            onLog: message => { if (logs.length < 16) logs.push(message.slice(0, 4096)); },
            onIoError: failure => { if (ioErrors.length < 16) ioErrors.push(failure); },
        });
        await pc.init(core, {memoryMiB: options.memoryMiB, cdBytes: options.cdBytes,
            cdFirst: true, serial: false, deterministic: false, readOnly: true,
            engine: options.engine, jitMiB: options.jitMiB, jitThreshold: options.jitThreshold});
        ready = true;
        await pc.loadRom(0, bios);
        await pc.loadRom(1, vga);
        initMs = elapsed();
        let nextFrame = performance.now() + options.frameMs, nextProgress = performance.now();
        stop = "step-cap";
        while (steps < options.stepCap) {
            if (elapsed() >= options.timeoutMs) { stop = "timeout"; break; }
            const beforeRun = performance.now();
            let status;
            slices++;
            try { status = await pc.run(Math.min(options.sliceSteps, options.stepCap - steps)); }
            finally { runMs += performance.now() - beforeRun; }
            if (status === 1) haltedSlices++;
            steps = pc.steps();
            if (ioErrors.length) throw new Error(`Disk I/O failed: ${JSON.stringify(ioErrors[0])}`);
            if (status < 0) throw new Error(pc.text(3));
            if (status === 2) { stop = "poweroff"; break; }
            if (performance.now() >= nextFrame) {
                nextFrame = performance.now() + options.frameMs;
                frame = pc.frame() ?? frame;
                polls++;
                if (frame?.width === 800 && frame.height === 600 && points.every(([x, y, r, g, b]) => {
                    const i = (y * 800 + x) * 4;
                    return frame.pixels[i] === r && frame.pixels[i + 1] === g && frame.pixels[i + 2] === b;
                })) { matched = true; stop = "frame-marker"; break; }
            }
            if (performance.now() >= nextProgress) {
                postMessage({type: "progress", ...sample()});
                nextProgress = performance.now() + 1000;
            }
            // Yield an event-loop turn even when HLT makes no guest progress.
            // Each new invocation publishes staged code before guest execution.
            await yieldTurn();
        }
        if (!matched) error = `Language-dialog frame not reached (${stop})`;
    } catch (failure) {
        // Direct exports such as frame() can trap outside WasmPc.invoke().
        if (pc && typeof WebAssembly !== "undefined" && failure instanceof WebAssembly.RuntimeError) pc.poisoned = true;
        error = `${failure.name ?? "Error"}: ${failure.message ?? failure}`;
    }
    const result = {...sample(), initMs, matched, stop, error, polls,
        io: io?.stats ?? null, ioErrors, logs};
    // A poisoned Wasm instance must never be reentered, even for diagnostics.
    if (ready && !pc.poisoned && !pc.busy) {
        try { jitCore = pc.text(4); if (error) diagnostics = pc.text(2); }
        catch (failure) { result.error ??= String(failure); }
    }
    result.peakMemoryBufferBytes = memoryBytes(); // Linear memory can only grow.
    result.jitCore = jitCore;
    result.diagnostics = diagnostics;
    channel.port1.close();
    channel.port2.close();
    postMessage({type: "result", result, frame}, frame ? [frame.pixels.buffer] : []);
}

let started = false;
self.onmessage = event => {
    if (!event.data?.start || started) return;
    started = true;
    run(event.data).catch(error => postMessage({type: "error", error: `${error.name}: ${error.message}`}));
};
