import {WasmJit} from "./jit.mjs";

/** The platform-independent host of the shared Rust interpreter and Wasm JIT.
 *
 * I/O returns bytes / undefined, or a Promise of those values. Only disk_io
 * can suspend. Asyncify saves the Wasm call stack, including live Rust borrows;
 * no other export may run until that invocation completes. In particular, UI
 * messages must be queued outside this object rather than reentering Rust.
 * A failed I/O resumes with an IDE error, never retries a guest instruction.
 * See Binaryen's src/passes/Asyncify.cpp for the public unwind/rewind ABI.
 */
export class WasmPc {
    constructor(io, {onLog = () => {}, onIoError = () => {}, onModule, now = () => performance.now()} = {}) {
        this.io = io;
        this.onLog = onLog;
        this.onIoError = onIoError;
        this.now = now;
        this.busy = false;
        this.suspensions = 0;
        this.decoder = new TextDecoder();
        this.jit = new WasmJit(this, {onModule});
    }

    async init(bytes, {memoryMiB = 128, diskSectors = 0, cdBytes = 0, cdFirst = true,
        serial = false, deterministic = false, readOnly = true, engine = "interp", jitMiB = 128, jitThreshold = 256} = {}) {
        if (!["interp", "jit"].includes(engine)) throw new Error("Invalid CPU engine");
        const imports = {browser: {
            disk_io: (...args) => this.diskImport(...args),
            monotonic_ms: this.now,
            epoch_ms: () => Date.now(),
            log: (ptr, len) => this.onLog(this.decode(ptr, len)),
            jit_compile: (...args) => this.jit.compile(...args),
            jit_retire: slot => this.jit.retire(slot),
        }};
        const {instance} = await WebAssembly.instantiate(bytes, imports);
        this.exports = instance.exports;
        const flags = Number(cdFirst) | (Number(serial) << 1) | (Number(deterministic) << 2) | (Number(readOnly) << 3);
        this.handle = this.exports.r86_create(memoryMiB, diskSectors, cdBytes, flags);
        if (!this.handle) throw new Error("Invalid machine configuration or insufficient Wasm memory");
        if (engine === "jit") {
            if (!this.exports.r86_jit_configure) throw new Error("Build the Wasm module with --jit first");
            this.jit.init();
            if (this.exports.r86_jit_configure(this.handle, jitMiB, jitThreshold) !== 0) throw new Error(this.text(3));
        }
        if (this.exports.asyncify_start_unwind) {
            this.asyncData = this.exports.r86_asyncify_data(this.handle);
        }
        return this;
    }

    view(ptr, len) { return new Uint8Array(this.exports.memory.buffer, ptr >>> 0, len); }
    decode(ptr, len) { return this.decoder.decode(this.view(ptr, len)); }

    idle() {
        if (this.poisoned) throw new Error("Wasm instance stopped after a trap");
        if (this.busy) throw new Error("Wasm machine reentry while an invocation is suspended");
    }

    diskImport(drive, operation, offset, ptr, len) {
        if (this.exports.asyncify_get_state?.() === 2) {
            this.exports.asyncify_stop_rewind();
            return this.ioResult;
        }
        const finish = result => {
            if (operation === 0) {
                if (!(result instanceof Uint8Array) || result.byteLength !== len) {
                    throw new Error("Short or invalid disk read");
                }
                this.view(ptr, len).set(result);
            }
            return 0;
        };
        const fail = error => {
            this.onIoError({drive, operation, offset, length: len, error: String(error)});
            return 1;
        };
        let result;
        try {
            // A write owns its copy until the transaction finishes. Read views
            // are created at completion: cached views may predate memory.grow.
            result = operation === 0 ? this.io.read(drive, offset, len) :
                operation === 1 ? this.io.write(drive, offset, this.view(ptr, len).slice()) :
                operation === 2 ? this.io.flush(drive) : (() => { throw new Error("Unknown disk operation"); })();
        } catch (error) { return fail(error); }
        if (!result || typeof result.then !== "function") {
            try { return finish(result); } catch (error) { return fail(error); }
        }
        if (!this.asyncData) {
            // This is a host configuration bug, not a guest disk error.
            result.catch(() => {});
            throw new Error("Asynchronous disk I/O requires an Asyncify build");
        }
        this.pending = Promise.resolve(result).then(finish).catch(fail);
        this.exports.asyncify_start_unwind(this.asyncData);
        this.suspensions++;
        return 0;
    }

    async invoke(name, ...args) {
        this.idle();
        this.busy = true;
        try {
            this.jit.publish();
            while (true) {
                const result = this.exports[name](this.handle, ...args);
                if (this.exports.asyncify_get_state?.() !== 1) return result;
                this.exports.asyncify_stop_unwind();
                this.ioResult = await this.pending;
                this.pending = undefined;
                this.exports.asyncify_start_rewind(this.asyncData);
            }
        } catch (error) {
            // A Wasm trap or host ABI bug is not a coverage miss. Its Rust
            // stack did not return normally; never reuse that instance.
            this.poisoned = true;
            throw error;
        } finally { this.busy = false; }
    }

    buffer(bytes) {
        this.idle();
        const ptr = this.exports.r86_buffer(this.handle, bytes.length);
        if (!ptr) throw new Error("Input buffer exceeds 1 MiB");
        this.view(ptr, bytes.length).set(bytes);
    }
    async loadRom(kind, bytes) {
        this.buffer(bytes);
        if (await this.invoke("r86_load_rom", kind) !== 0) throw new Error(this.text(3));
    }
    run(budget, untilSerial = false) { return this.invoke("r86_run", budget, Number(untilSerial)); }
    async control(action) {
        if (await this.invoke("r86_control", action) !== 0) throw new Error(this.text(3));
    }
    steps() { this.idle(); return this.exports.r86_steps(this.handle); }
    // Bytes from absolute position `from`; r86 keeps at least the last 64 KiB.
    serial(from = 0) {
        this.idle();
        const len = this.exports.r86_serial_len(this.handle);
        const ptr = this.exports.r86_serial(this.handle);
        const skip = Math.min(Math.max(0, from - this.exports.r86_serial_start(this.handle)), len);
        return this.view(ptr + skip, len - skip).slice();
    }
    text(kind) {
        this.idle();
        const ptr = this.exports.r86_text(this.handle, kind);
        return this.decode(ptr, this.exports.r86_text_len(this.handle));
    }
    frame() {
        this.idle();
        const ptr = this.exports.r86_frame(this.handle);
        if (!ptr) return null;
        const dimensions = this.exports.r86_dimensions(this.handle);
        const width = dimensions & 65535, height = dimensions >>> 16;
        return {width, height, pixels: this.view(ptr, width * height * 4).slice()};
    }
}
