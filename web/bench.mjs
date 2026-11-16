const $ = id => document.getElementById(id);
// Match run_wasm.py; the single --jit core also executes the interpreter.
const settings = {memoryMiB: 128, clock: "realtime", stepCap: 100000000000,
    timeoutMs: 600000, sliceSteps: 100000, frameMs: 100, jitMiB: 128, jitThreshold: 256,
    chunkBytes: 256 * 1024, cacheBytes: 32 * 1024 ** 2};
const number = value => Number.isFinite(value) ? value.toFixed(2) : "—";
let running = false;

// Both probes have one (i32) -> i32 type and export their last function.
// All section/body sizes here fit a one-byte unsigned LEB.
function probeModule(bodies) {
    const section = (id, bytes) => [id, bytes.length, ...bytes];
    return new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0,
        ...section(1, [1, 0x60, 1, 0x7f, 1, 0x7f]),
        ...section(3, [bodies.length, ...bodies.map(() => 0)]),
        ...section(7, [1, 3, 114, 117, 110, 0, bodies.length - 1]),
        ...section(10, [bodies.length, ...bodies.flatMap(body => [body.length, ...body])])]);
}

async function probe() {
    const result = {webAssemblyType: typeof WebAssembly, tailCalls: false, tailError: null,
        loopIterations: 50000000, loopMs: null, loopChecksum: null, loopError: null};
    if (typeof WebAssembly === "undefined") {
        result.tailError = result.loopError = "WebAssembly is unavailable";
        return result;
    }
    try {
        // (func $id (param i32) (result i32) local.get 0)
        // (func (param i32) (result i32) local.get 0 return_call $id)
        const module = await WebAssembly.compile(probeModule([
            [0, 0x20, 0, 0x0b], [0, 0x20, 0, 0x12, 0, 0x0b],
        ]));
        result.tailCalls = new WebAssembly.Instance(module).exports.run(7) === 7;
    } catch (error) { result.tailError = String(error); }
    try {
        // acc = (acc * 3) XOR n; loop until --n == 0. Fixed work, roughly
        // 50 ms on a fast host; report actual time, with no calibration/warmup.
        const module = await WebAssembly.compile(probeModule([[
            1, 1, 0x7f, 0x03, 0x40, 0x20, 1, 0x41, 3, 0x6c, 0x20, 0, 0x73, 0x21, 1,
            0x20, 0, 0x41, 1, 0x6b, 0x22, 0, 0x0d, 0, 0x0b, 0x20, 1, 0x0b,
        ]]));
        const instance = new WebAssembly.Instance(module);
        const before = performance.now();
        result.loopChecksum = instance.exports.run(result.loopIterations);
        result.loopMs = performance.now() - before;
    } catch (error) { result.loopError = String(error); }
    return result;
}

function showRun(run, pending = false) {
    $(`${run.engine}-number`).textContent = number(run.busyMips);
    $(`${run.engine}-note`).textContent = `${number(run.wallMs)} ms wall · ${number(run.mips)} wall MIPS\n` +
        (pending ? `${run.steps.toLocaleString()} steps; running` : run.error ?? (run.matched ? "Frame matched" : run.stop));
}

function runOne(engine, media) {
    return new Promise(resolve => {
        const startedAt = performance.timeOrigin + performance.now();
        let worker, done = false;
        let latest = {engine, steps: 0, readMs: 0, runMs: 0, haltedSlices: 0, slices: 0,
            busyMips: null, mips: null, jit: null, peakMemoryBufferBytes: 0};
        const fallback = error => {
            const wallMs = performance.timeOrigin + performance.now() - startedAt;
            return {...latest, matched: false, error, stop: "worker-error", lastProgressOnly: true, wallMs,
                mips: wallMs > latest.readMs ? latest.steps / ((wallMs - latest.readMs) * 1000) : null};
        };
        const timeout = setTimeout(() => finish(fallback("Worker timed out or stopped responding")), settings.timeoutMs + 30000);
        function finish(result) {
            if (done) return;
            done = true;
            clearTimeout(timeout);
            worker?.terminate();
            resolve(result);
        }
        try {
            worker = new Worker("/worker.mjs", {type: "module"});
            worker.onerror = event => {
                event.preventDefault();
                finish(fallback(event.message || "Worker could not load"));
            };
            worker.onmessageerror = () => finish(fallback("Could not receive Worker result"));
            worker.onmessage = ({data}) => {
                if (data.type === "progress") {
                    latest = data;
                    showRun(data, true);
                } else if (data.type === "result") {
                    if (data.frame) {
                        try {
                            const {width, height, pixels} = data.frame;
                            const canvas = $("frame");
                            canvas.width = width;
                            canvas.height = height;
                            canvas.hidden = false;
                            canvas.getContext("2d").putImageData(new ImageData(new Uint8ClampedArray(pixels.buffer), width, height), 0, 0);
                        } catch (error) { data.result.previewError = String(error); }
                    }
                    finish(data.result);
                } else if (data.type === "error") finish(fallback(data.error));
            };
            worker.postMessage({start: true, engine, ...settings, cdBytes: media.bytes, startedAt});
        } catch (error) { finish(fallback(String(error))); }
    });
}

async function getJson(url) {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
    return response.json();
}

async function save(report) {
    delete report.savedFile;
    delete report.saveError;
    $("result").textContent = JSON.stringify(report, null, 2);
    try {
        const response = await fetch("/result", {method: "POST", headers: {"Content-Type": "application/json"},
            body: JSON.stringify(report)});
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        report.savedFile = (await response.json()).file;
    } catch (error) { report.saveError = String(error); }
    $("result").textContent = JSON.stringify(report, null, 2);
}

async function run(engines) {
    if (running) return;
    running = true;
    for (const button of document.querySelectorAll("button")) button.disabled = true;
    for (const engine of ["interp", "jit"]) {
        $(`${engine}-number`).textContent = "—";
        $(`${engine}-note`).textContent = "Not run";
    }
    $("ratio").textContent = "—";
    $("frame").hidden = true;
    const report = {schema: 1, started: new Date().toISOString(), userAgent: navigator.userAgent,
        settings, results: [], complete: false, busyRatio: null, wallMipsRatio: null};
    try {
        $("status").textContent = "Checking WebAssembly, tail calls and a fixed Wasm loop…";
        report.probe = await probe();
        $("probe").textContent = `WebAssembly: ${report.probe.webAssemblyType} · tail calls: ${report.probe.tailCalls ? "yes" : report.probe.tailError ?? "no"} · ` +
            `50M loop: ${report.probe.loopError ?? `${number(report.probe.loopMs)} ms`}`;
        const [config, build] = await Promise.all([getJson("/config.json"), getJson("/dist/build.json")]);
        report.build = build;
        report.artifact = {name: "r86-jit.wasm", ...build.artifacts["r86-jit.wasm"]};
        report.media = config.media;
        report.firmware = config.firmware;
        $("artifact").textContent = `${config.media.name} · core SHA256 ${report.artifact.sha256}`;
        for (const engine of engines) {
            $("status").textContent = `Running ${engine} to the language dialog. Keep this page in the foreground…`;
            const result = await runOne(engine, config.media);
            report.results.push(result);
            showRun(result);
            const interp = report.results.find(run => run.engine === "interp");
            const jit = report.results.find(run => run.engine === "jit");
            if (interp?.matched && jit?.matched && !interp.error && !jit.error) {
                report.busyRatio = interp.busyMips > 0 ? jit.busyMips / interp.busyMips : null;
                report.wallMipsRatio = interp.mips > 0 ? jit.mips / interp.mips : null;
                $("ratio").textContent = `${number(report.busyRatio)}×`;
            }
            report.complete = report.results.length === engines.length;
            await save(report);
        }
        $("status").textContent = report.saveError ? `Save failed: ${report.saveError}. JSON is below.` :
            `Finished. Saved ${report.savedFile}.`;
    } catch (error) {
        report.error = String(error);
        report.complete = true;
        await save(report);
        $("status").textContent = `Stopped: ${report.error}${report.saveError ? `; save failed: ${report.saveError}` : " (result saved)"}`;
    } finally {
        // Also available to the dependency-free Chromium check through CDP.
        window.w0Result = report;
        running = false;
        for (const button of document.querySelectorAll("button")) button.disabled = false;
    }
}

for (const button of document.querySelectorAll("button")) button.onclick = () => run(button.dataset.engines.split(","));
if (new URLSearchParams(location.search).get("auto") === "1") run(["interp", "jit"]);
