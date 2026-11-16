// The native app owns input and sizing. This page only probes the web engine,
// transfers the canvas, and relays the protocol in src/frontend/web_host.rs.
const base = new URL("../", location.href);
const ctl = new URL("ctl", base);
ctl.protocol = "ws:";
const socket = new WebSocket(ctl);
let worker, stopped = false;
const send = text => { if (!stopped && socket.readyState === WebSocket.OPEN) socket.send(text); };
const reason = error => String(error).replace(/[\r\n]/g, " ").slice(0, 2048);

function stop() {
    stopped = true;
    worker?.terminate();
    worker = undefined;
    socket.close();
}

// Same fixed loop and return_call probe as bench.mjs. This product page is
// packaged separately from the benchmark; neither requires the other.
function probeModule(bodies) {
    const section = (id, bytes) => [id, bytes.length, ...bytes];
    return new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0,
        ...section(1, [1, 0x60, 1, 0x7f, 1, 0x7f]),
        ...section(3, [bodies.length, ...bodies.map(() => 0)]),
        ...section(7, [1, 3, 114, 117, 110, 0, bodies.length - 1]),
        ...section(10, [bodies.length, ...bodies.flatMap(body => [body.length, ...body])])]);
}

async function probe() {
    if (typeof WebAssembly === "undefined") throw new Error("WebAssembly is unavailable");
    const tail = await WebAssembly.compile(probeModule([
        [0, 0x20, 0, 0x0b], [0, 0x20, 0, 0x12, 0, 0x0b],
    ]));
    if (new WebAssembly.Instance(tail).exports.run(7) !== 7) throw new Error("return_call failed");
    const loop = await WebAssembly.compile(probeModule([[
        1, 1, 0x7f, 0x03, 0x40, 0x20, 1, 0x41, 3, 0x6c, 0x20, 0, 0x73, 0x21, 1,
        0x20, 0, 0x41, 1, 0x6b, 0x22, 0, 0x0d, 0, 0x0b, 0x20, 1, 0x0b,
    ]]));
    const instance = new WebAssembly.Instance(loop);
    const before = performance.now();
    instance.exports.run(50000000);
    const ms = performance.now() - before;
    if (ms > 500) throw new Error(`WebAssembly probe took ${ms.toFixed(1)} ms (limit 500)`);
    return ms;
}

socket.onopen = async () => {
    try {
        const ms = await probe();
        const canvas = document.getElementById("screen").transferControlToOffscreen();
        if (stopped) return;
        send(`probe 1 ${ms}`);
        worker = new Worker(new URL("vm-worker.mjs", import.meta.url), {type: "module"});
        worker.onmessage = ({data}) => {
            send(data);
            if (data === "off" || data.startsWith("error ")) stop();
        };
        worker.onerror = event => {
            event.preventDefault();
            send(`error ${reason(event.message || "Web Worker failed")}`);
            stop();
        };
        worker.onmessageerror = () => { send("error Invalid Worker message"); stop(); };
        worker.postMessage({base: base.href, canvas}, [canvas]);
    } catch (error) {
        send(`probe 0 ${reason(error)}`);
        stop();
    }
};
socket.onmessage = ({data}) => {
    if (data === "stop") stop();
    else worker?.postMessage(data);
};
socket.onclose = stop;
socket.onerror = stop;
