#!/usr/bin/env node
"use strict";
// Act 0 diagnostic server. Only the names below are served; request paths
// never become filesystem paths. The ISO is read-only. No dependencies.
const fs = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const {createHash} = require("node:crypto");
const {parseArgs} = require("node:util");
const root = path.resolve(__dirname, "..");

async function sha256(file) {
    const hash = createHash("sha256");
    for await (const bytes of fs.createReadStream(file)) hash.update(bytes);
    return hash.digest("hex");
}

function range(header, size) {
    const match = /^bytes=(\d*)-(\d*)$/.exec(header);
    if (!match || (!match[1] && !match[2])) return null;
    let start = Number(match[1]), end = match[2] ? Number(match[2]) : size - 1;
    if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end)) return null;
    if (!match[1]) {
        if (end === 0) return null;
        start = Math.max(0, size - end);
        end = size - 1;
    }
    return start <= end && start < size ? [start, Math.min(end, size - 1)] : null;
}

function reply(res, status, text, type = "text/plain; charset=utf-8") {
    res.writeHead(status, {"Content-Type": type, "Content-Length": Buffer.byteLength(text)});
    res.end(text);
}

async function saveResult(req, res) {
    if (req.headers["content-type"]?.split(";")[0] !== "application/json") {
        reply(res, 415, "Expected application/json\n");
        return;
    }
    const chunks = [];
    let size = 0;
    for await (const chunk of req) {
        size += chunk.length;
        if (size > 1024 * 1024) {
            reply(res, 413, "Result exceeds 1 MiB\n");
            return;
        }
        chunks.push(chunk);
    }
    let result;
    try { result = JSON.parse(Buffer.concat(chunks).toString("utf8")); }
    catch { reply(res, 400, "Invalid JSON\n"); return; }
    if (!result || typeof result !== "object" || Array.isArray(result)) {
        reply(res, 400, "Expected a JSON object\n");
        return;
    }
    const directory = path.join(root, "tmp/wkjit/results");
    await fs.promises.mkdir(directory, {recursive: true});
    const name = `${new Date().toISOString().replace(/[:.]/g, "-")}-${process.hrtime.bigint()}.json`;
    await fs.promises.writeFile(path.join(directory, name), JSON.stringify(result, null, 2) + "\n", {flag: "ax"});
    const runs = Array.isArray(result.results) ? result.results : [];
    const summary = runs.map(run => ({engine: run.engine, matched: run.matched,
        busyMips: run.busyMips, mips: run.mips, wallMs: run.wallMs, error: run.error}));
    console.log(`W0_RESULT ${JSON.stringify({file: name, complete: result.complete, runs: summary})}`);
    reply(res, 201, JSON.stringify({file: name}) + "\n", "application/json");
}

async function main() {
    const {values: args} = parseArgs({options: {
        iso: {type: "string"}, port: {type: "string", default: "8186"},
        lan: {type: "boolean"}, help: {type: "boolean"},
    }});
    if (args.help) {
        console.log("Usage: node script/wasm_serve.cjs --iso refs/ReactOS.iso [--lan] [--port 8186]");
        return;
    }
    const port = Number(args.port);
    if (!args.iso || !Number.isInteger(port) || port < 1 || port > 65535) {
        throw new Error("Supply --iso FILE and a port from 1 to 65535; see --help");
    }
    const iso = path.resolve(args.iso);
    const size = fs.statSync(iso).size;
    if (!Number.isSafeInteger(size) || size < 2048 || size > 0xffffffff || size % 2048) {
        throw new Error("ISO must be nonempty, 2048-byte aligned and smaller than 4 GiB");
    }
    const files = new Map([
        ["/", "web/bench.html"], ["/bench.html", "web/bench.html"],
        ["/bench.mjs", "web/bench.mjs"], ["/worker.mjs", "web/worker.mjs"],
        ["/wasm.mjs", "web/wasm.mjs"], ["/jit.mjs", "web/jit.mjs"],
        ["/dist/r86-jit.wasm", "web/dist/r86-jit.wasm"], ["/dist/build.json", "web/dist/build.json"],
        ["/bios.bin", "firmware/seabios/bios.bin"],
        ["/vgabios-stdvga.bin", "firmware/seabios/vgabios-stdvga.bin"],
        ["/langsel.rgb", "benchmarks/reactos_livecd_boot_to_langsel/langsel.rgb"],
        ["/media.iso", iso],
    ].map(([url, file]) => [url, path.resolve(root, file)]));
    for (const file of files.values()) if (!fs.statSync(file).isFile()) throw new Error(`Not a file: ${file}`);
    const build = JSON.parse(fs.readFileSync(files.get("/dist/build.json"), "utf8"));
    const artifact = build.artifacts?.["r86-jit.wasm"];
    if (!artifact || artifact.sha256 !== await sha256(files.get("/dist/r86-jit.wasm"))) {
        throw new Error("build.json does not match r86-jit.wasm; rebuild with script/build_wasm.py --jit");
    }
    const config = JSON.stringify({media: {name: path.basename(iso), bytes: size, sha256: await sha256(iso)},
        firmware: {bios: await sha256(files.get("/bios.bin")), vga: await sha256(files.get("/vgabios-stdvga.bin"))}});
    const mime = {".html": "text/html; charset=utf-8", ".mjs": "text/javascript; charset=utf-8",
        ".json": "application/json", ".wasm": "application/wasm", ".rgb": "text/plain; charset=utf-8"};
    const server = http.createServer(async (req, res) => {
        res.setHeader("Cache-Control", "no-store");
        res.setHeader("X-Content-Type-Options", "nosniff");
        const url = req.url.split("?")[0];
        try {
            if (url === "/result" && req.method === "POST") return await saveResult(req, res);
            if (req.method !== "GET" && req.method !== "HEAD") return reply(res, 405, "Use GET or HEAD\n");
            if (url === "/config.json") {
                res.writeHead(200, {"Content-Type": "application/json", "Content-Length": Buffer.byteLength(config)});
                res.end(req.method === "HEAD" ? undefined : config);
                return;
            }
            const file = files.get(url);
            if (!file) return reply(res, 404, "Not found\n");
            const length = fs.statSync(file).size;
            res.setHeader("Accept-Ranges", "bytes");
            let start = 0, end = length - 1, status = 200;
            if (req.headers.range !== undefined) {
                const selected = range(req.headers.range, length);
                if (!selected) {
                    res.setHeader("Content-Range", `bytes */${length}`);
                    return reply(res, 416, "Unsatisfiable range\n");
                }
                [start, end] = selected;
                status = 206;
                res.setHeader("Content-Range", `bytes ${start}-${end}/${length}`);
            }
            res.writeHead(status, {"Content-Type": mime[path.extname(file)] ?? "application/octet-stream",
                "Content-Length": end - start + 1});
            if (req.method === "HEAD") return res.end();
            const stream = fs.createReadStream(file, {start, end});
            stream.on("error", error => res.destroy(error));
            res.on("close", () => stream.destroy());
            stream.pipe(res);
        } catch (error) {
            console.error(error);
            if (!res.headersSent) reply(res, 500, "Server error\n");
            else res.destroy();
        }
    });
    server.on("error", error => { console.error(error); process.exitCode = 1; });
    server.listen(port, args.lan ? "0.0.0.0" : "127.0.0.1", () => {
        console.log(`http://127.0.0.1:${port}/bench.html?auto=1`);
        if (args.lan) for (const entries of Object.values(os.networkInterfaces())) {
            for (const address of entries ?? []) if (address.family === "IPv4" && !address.internal) {
                console.log(`http://${address.address}:${port}/bench.html?auto=1`);
            }
        }
        console.log(`ISO ${path.basename(iso)}; results: tmp/wkjit/results/; Ctrl+C to stop.`);
    });
}

main().catch(error => { console.error(error.message); process.exitCode = 1; });
