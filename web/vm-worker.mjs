import {WasmPc} from "./wasm.mjs";

// All disk requests are synchronous on this Worker. A failed request throws
// into WasmPc's disk import, which returns a guest IDE error. The cache holds
// logical bytes; VHD allocation and frozen overlays belong to the native host.
class Disks {
    constructor(base, sizes) {
        this.base = base;
        this.sizes = sizes;
        this.chunkBytes = 256 * 1024;
        this.capBytes = 64 * 1024 ** 2;
        this.cache = new Map();
        this.cacheBytes = 0;
        this.stats = {calls: 0, bytes: 0, readMs: 0, maxMs: 0, requests: 0, xhrMs: 0, xhrMaxMs: 0};
    }
    request(method, path, body, range) {
        const xhr = new XMLHttpRequest();
        xhr.open(method, new URL(path, this.base), false);
        xhr.responseType = "arraybuffer";
        if (range) xhr.setRequestHeader("Range", `bytes=${range[0]}-${range[1]}`);
        const before = performance.now();
        try { xhr.send(body); }
        finally {
            if (range) {
                const ms = performance.now() - before;
                this.stats.requests++;
                this.stats.xhrMs += ms;
                this.stats.xhrMaxMs = Math.max(this.stats.xhrMaxMs, ms);
            }
        }
        if (xhr.status !== (range ? 206 : 204)) throw new Error(`Disk ${method}: HTTP ${xhr.status}`);
        return xhr;
    }
    chunk(drive, start) {
        const key = `${drive}:${start}`;
        let bytes = this.cache.get(key);
        if (bytes) {
            this.cache.delete(key);
            this.cache.set(key, bytes);
            return bytes;
        }
        const end = Math.min(start + this.chunkBytes, this.sizes[drive]) - 1;
        const xhr = this.request("GET", `disk/${drive}`, null, [start, end]);
        if (xhr.getResponseHeader("Content-Range") !== `bytes ${start}-${end}/${this.sizes[drive]}` ||
            !(xhr.response instanceof ArrayBuffer) || xhr.response.byteLength !== end - start + 1) {
            throw new Error("Invalid disk range response");
        }
        bytes = new Uint8Array(xhr.response);
        while (this.cacheBytes + bytes.length > this.capBytes) this.drop(this.cache.keys().next().value);
        this.cache.set(key, bytes);
        this.cacheBytes += bytes.length;
        return bytes;
    }
    drop(key) {
        this.cacheBytes -= this.cache.get(key)?.length ?? 0;
        this.cache.delete(key);
    }
    bounds(drive, offset, length) {
        if (![0, 1].includes(drive) || !Number.isSafeInteger(offset) || !Number.isSafeInteger(length) ||
            offset < 0 || length < 1 || offset + length > this.sizes[drive]) throw new Error("Disk access out of bounds");
    }
    read(drive, offset, length) {
        const before = performance.now();
        this.stats.calls++;
        this.stats.bytes += length;
        try {
            this.bounds(drive, offset, length);
            const bytes = new Uint8Array(length);
            for (let copied = 0; copied < length;) {
                const position = offset + copied;
                const start = Math.floor(position / this.chunkBytes) * this.chunkBytes;
                const chunk = this.chunk(drive, start);
                const count = Math.min(length - copied, chunk.length - (position - start));
                bytes.set(chunk.subarray(position - start, position - start + count), copied);
                copied += count;
            }
            return bytes;
        } finally {
            const ms = performance.now() - before;
            this.stats.readMs += ms;
            this.stats.maxMs = Math.max(this.stats.maxMs, ms);
        }
    }
    write(drive, offset, bytes) {
        this.bounds(drive, offset, bytes.length);
        if (drive !== 0) throw new Error("CD is read-only");
        let written = false;
        try {
            this.request("POST", `disk/${drive}?o=${offset}`, bytes);
            written = true;
        } finally {
            for (let start = Math.floor(offset / this.chunkBytes) * this.chunkBytes;
                start < offset + bytes.length; start += this.chunkBytes) {
                const key = `${drive}:${start}`;
                if (!written) {
                    // A failed native write may have partially succeeded.
                    this.drop(key);
                } else {
                    const chunk = this.cache.get(key);
                    if (chunk) {
                        const from = Math.max(offset, start);
                        const end = Math.min(offset + bytes.length, start + chunk.length);
                        chunk.set(bytes.subarray(from - offset, end - offset), from - start);
                    }
                }
            }
        }
    }
    flush(drive) { this.request("POST", `disk/${drive}/flush`, null); }
}

async function get(url, json = false) {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`Load failed: HTTP ${response.status}`);
    return json ? response.json() : new Uint8Array(await response.arrayBuffer());
}

let started = false, wake;
const commands = [];
self.onmessage = ({data}) => {
    if (!started && typeof data === "object") {
        started = true;
        run(data).catch(error => postMessage(`error ${String(error).replace(/[\r\n]/g, " ").slice(0, 2048)}`));
    } else if (typeof data === "string") {
        commands.push(data);
        wake?.();
    }
};

async function run({base, canvas}) {
    const context = canvas.getContext("2d", {alpha: false});
    if (!context) throw new Error("OffscreenCanvas 2D is unavailable");
    const [config, core, bios, vga] = await Promise.all([
        get(new URL("config", base), true), get(new URL("app/r86-jit.wasm", base)),
        get(new URL("rom/0", base)), get(new URL("rom/1", base)),
    ]);
    const disks = new Disks(base, [config.diskSectors * 512, config.cdBytes]);
    let ioMessages = 0;
    const pc = new WasmPc(disks, {
        onLog: text => console.log(text),
        onIoError: failure => {
            if (ioMessages++ === 0) postMessage(`message Disk I/O failed: ${failure.error}`);
        },
    });
    await pc.init(core, {...config, engine: "jit", deterministic: false, serial: false});
    await pc.loadRom(0, bios);
    await pc.loadRom(1, vga);
    await pc.invoke("r86_app_configure", ...config.recommended);

    // Runnable turns use a MessageChannel (no timer clamp). HLT waits for the
    // core's next timer deadline; native commands wake both HLT and pause.
    const channel = new MessageChannel();
    let turn;
    channel.port1.onmessage = () => { const done = turn; turn = undefined; done?.(); };
    const yieldTurn = () => new Promise(resolve => { turn = resolve; channel.port2.postMessage(null); });
    const sleep = ms => new Promise(resolve => {
        const done = () => { clearTimeout(timer); wake = undefined; resolve(); };
        const timer = setTimeout(done, ms);
        wake = done;
    });
    let paused = false, width = 0, height = 0, nextFrame = 0;
    let lastReport = performance.now(), lastSteps = 0;
    let lastYield = lastReport;
    const report = () => {
        const now = performance.now(), steps = pc.steps(), elapsed = now - lastReport;
        const ips = paused || elapsed <= 0 ? 0 : Math.round(Math.max(0, steps - lastSteps) * 1000 / elapsed);
        const pointer = pc.exports.r86_pointer_active(pc.handle);
        const s = disks.stats, jit = pc.jit.stats;
        postMessage(`status ${Number(paused)} ${ips} ${steps} ${width} ${height} ${pointer} ${jit.published} ` +
            `${jit.compilation_latency_ms} ${s.calls} ${s.bytes} ${s.readMs} ${s.maxMs} ${s.requests} ${s.xhrMs} ${s.xhrMaxMs}`);
        lastSteps = steps;
        lastReport = now;
    };
    postMessage("ready");
    try {
        while (true) {
            for (const text of commands.splice(0)) {
                const [kind, ...fields] = text.split(" ");
                const args = fields.map(Number);
                if (args.some(n => !Number.isSafeInteger(n))) throw new Error("Invalid native command");
                if (kind === "control" && args.length === 1) {
                    if (await pc.invoke("r86_app_control", ...args) !== 0) throw new Error(pc.text(3));
                    if (args[0] === 0 || args[0] === 1) paused = args[0] === 0;
                } else if (kind === "key" && args.length === 2) await pc.invoke("r86_key", ...args);
                else if (kind === "mouse" && args.length === 3) await pc.invoke("r86_mouse", ...args);
                else if (kind === "pointer" && args.length === 4) await pc.invoke("r86_pointer", ...args);
                else throw new Error("Unknown native command");
            }
            // Batch runnable slices for 8 ms before servicing the page.
            let status = 1;
            if (!paused) {
                do { status = await pc.run(100000); }
                while (status === 0 && performance.now() - lastYield < 8);
            }
            if (status < 0) throw new Error(pc.text(3));
            if (status === 2) {
                await pc.control(2); // flush before the power-off notification
                postMessage("off");
                return;
            }
            const now = performance.now();
            if (now >= nextFrame) {
                nextFrame = now + 1000 / 30;
                const frame = pc.frame();
                if (frame) {
                    if (width !== frame.width || height !== frame.height) {
                        canvas.width = width = frame.width;
                        canvas.height = height = frame.height;
                    }
                    context.putImageData(new ImageData(new Uint8ClampedArray(frame.pixels.buffer), width, height), 0, 0);
                }
            }
            if (now - lastReport >= 500) report();
            const wait = Math.min(500 - (performance.now() - lastReport),
                paused ? 500 : status === 1 ? pc.exports.r86_halted_wait_ms(pc.handle) : 0);
            if (!commands.length && wait > 0) {
                await sleep(wait);
                lastYield = performance.now();
            } else if (performance.now() - lastYield >= 8) {
                await yieldTurn();
                lastYield = performance.now();
            }
        }
    } finally {
        channel.port1.close();
        channel.port2.close();
    }
}
