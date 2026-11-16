// Node --import observer for the unchanged wasm_cli.cjs. No guest semantics change.
import {pathToFileURL} from 'node:url';
const {WasmPc} = await import(pathToFileURL(process.env.R86_LOOP_HOST_MODULE));
const run = WasmPc.prototype.run;
let seen = 0, pending = '';
const starts = new Map();
WasmPc.prototype.run = async function (...args) {
    const status = await run.apply(this, args);
    const bytes = this.serial(seen);
    seen += bytes.length;
    pending += Buffer.from(bytes).toString('ascii');
    let end;
    while ((end = pending.indexOf('\n')) >= 0) {
        const line = pending.slice(0, end);
        pending = pending.slice(end + 1);
        const match = /^L1 (P[1-4]) (START|END) ([0-9A-F]{16})$/.exec(line);
        if (match) {
            const now = process.hrtime.bigint();
            const record = {phase: match[1], edge: match[2], host_ns: now.toString(),
                step: this.steps(), core: this.text(4), host: this.jit.stats};
            if (match[2] === 'END') record.wall_ns = (now - starts.get(match[1])).toString();
            process.stderr.write('L1_OBS ' + JSON.stringify(record) + '\n');
            // Keep the start snapshot/formatting outside the measured interval.
            if (match[2] === 'START') starts.set(match[1], process.hrtime.bigint());
        }
    }
    return status;
};
