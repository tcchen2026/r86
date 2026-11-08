/** Host ownership of generated modules. One staged module, no executable
 * cache files, no generated-module imports of other generated functions. The
 * core table is the sole persistent owner of a published region's function.
 * Publication occurs at the next idle machine boundary, after core dependency
 * validation. Bounded modules use V8's ordinary lazy/baseline policy; optimizing
 * tiers remain the engine's responsibility.
 */
export class WasmJit {
    constructor(pc, {onModule = undefined} = {}) {
        this.pc = pc;
        this.onModule = onModule;
        this.stats = {submitted: 0, published: 0, discarded: 0, retired: 0,
            live_bytecode: 0, peak_live_bytecode: 0, live_functions: 0,
            peak_live_functions: 0, compilation_latency_ms: 0};
    }
    init() {
        this.table = this.pc.exports.__indirect_function_table;
        if (!this.table) throw new Error("Missing Wasm JIT function table");
        this.groupSize = this.pc.exports.r86_jit_group_size();
        this.sizes = new Uint32Array(this.pc.exports.r86_jit_slots() / this.groupSize);
        this.functions = new Uint8Array(this.sizes.length);
        this.base = this.table.grow(this.sizes.length * this.groupSize);
    }
    compile(slot, request, ptr, len) {
        if (this.pending) return 0;
        if (slot >= this.sizes.length || len > 65 * 1024) throw new Error("Invalid generated module submission");
        const bytes = this.pc.view(ptr, len).slice();
        this.onModule?.(bytes, {slot, request});
        const job = {slot, request, bytes: len, started: performance.now()};
        this.pending = job;
        this.stats.submitted++;
        // The core deferred admission until this entry was hot. Construct a
        // bounded module now and publish as soon as the core yields. Continuing
        // a long interpreted slice after async submission delayed useful code.
        job.module = new WebAssembly.Module(bytes);
        this.stats.compilation_latency_ms += performance.now() - job.started;
        return 1;
    }
    publish() {
        const job = this.pending;
        if (!job?.module) return;
        const instance = new WebAssembly.Instance(job.module, {r: {
            memory: this.pc.exports.memory,
            table: this.table,
        }});
        const functionIndex = this.base + job.slot * this.groupSize;
        if (this.pc.exports.r86_jit_complete(this.pc.handle, job.request, functionIndex)) {
            let count = 0;
            for (; count < this.groupSize; count++) {
                const f = instance.exports[count ? `run${count}` : "run"];
                if (!f) break;
                this.table.set(functionIndex + count, f);
            }
            this.functions[job.slot] = count;
            this.stats.live_functions += count;
            this.stats.peak_live_functions = Math.max(this.stats.peak_live_functions, this.stats.live_functions);
            this.stats.live_bytecode += job.bytes - this.sizes[job.slot];
            this.sizes[job.slot] = job.bytes;
            this.stats.peak_live_bytecode = Math.max(this.stats.peak_live_bytecode, this.stats.live_bytecode);
            this.stats.published++;
        } else {
            this.stats.discarded++;
        }
        this.pending = undefined;
    }
    retire(slot) {
        if (slot >= this.sizes.length) throw new Error("Invalid Wasm JIT retirement");
        for (let i = 0; i < this.functions[slot]; i++) this.table.set(this.base + slot * this.groupSize + i, null);
        this.stats.live_functions -= this.functions[slot];
        this.functions[slot] = 0;
        this.stats.live_bytecode -= this.sizes[slot];
        this.sizes[slot] = 0;
        this.stats.retired++;
    }
}
