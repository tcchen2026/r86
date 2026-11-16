//! Native disk owner for the iOS web engine. One event thread owns all media;
//! connection threads only parse HTTP/WS and transfer owned byte buffers.
//!
//! Protocol (same-origin, token is the first path component):
//! GET app/{vm.html,vm.mjs,vm-worker.mjs,wasm.mjs,jit.mjs,r86-jit.wasm},
//! GET config, GET rom/{0,1}; GET disk/{0,1} with one byte Range (<=1 MiB),
//! POST disk/0?o=byte_offset with sector-aligned bytes, POST disk/{0,1}/flush.
//! Disk 0 is logical HDD sectors (VHD/frozen handled here), 1 is read-only CD.
//! HTTP connections are reused until peer close or 5 s idle (8 connections max).
//! WS ctl text, one message per line (numbers decimal, flags 0/1):
//! host -> page: control ACTION (C API's 0..5), key CODE DOWN (keys::wire_code),
//! mouse DX DY BUTTONS, pointer X Y BUTTONS WHEEL, stop.
//! page -> host: probe 0 REASON | probe 1 LOOP_MS; ready; message TEXT;
//! error TEXT; off; status STATE IPS STEPS W H POINTER MODULES COMPILE_MS
//! READ_CALLS READ_BYTES READ_MS READ_MAX_MS HTTP_READS HTTP_READ_MS HTTP_MAX_MS.
//! STATE uses runner::State. Extra status counters are diagnostics; IPS/state/
//! dimensions/pointer feed Shared. Unknown or malformed messages fail the run.

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::runner::{Command, Shared, State, lock};
use super::{run_log, vm_store, ws};
use r86::disk::Disk;
use r86::machine::DisplayMode;
use vm_store::Vm;

const MAX_BODY: usize = 1024 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Link {
    pub url: String,
    pub state: Arc<AtomicU32>,
    pub report: Arc<Mutex<String>>,
    events: mpsc::SyncSender<Event>,
}
impl Link {
    pub fn wake(&self) {
        let _ = self.events.try_send(Event::Wake);
    }
}

enum Event {
    Wake,
    Http(ws::HttpRequest, mpsc::SyncSender<Response>),
    Connected(ws::WebSocket),
    Text(String),
    Disconnected,
    Failed(String),
}

struct Response {
    status: &'static str,
    mime: &'static str,
    headers: String,
    body: Vec<u8>,
}
impl Response {
    fn new(status: &'static str, mime: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            mime,
            headers: String::new(),
            body,
        }
    }
    fn error(status: &'static str, message: impl ToString) -> Self {
        Self::new(
            status,
            "text/plain; charset=utf-8",
            message.to_string().into_bytes(),
        )
    }
}

struct Media {
    disk: Disk,
    cd: Option<File>,
    cd_bytes: u64,
    config: Vec<u8>,
    assets: HashMap<String, (&'static str, Vec<u8>)>,
}

impl Media {
    fn open(
        vm: &Vm,
        firmware: &std::path::Path,
        recommended: Option<DisplayMode>,
        frozen: bool,
    ) -> Result<Self, String> {
        vm.validate()?;
        if vm.memory > 512 {
            return Err("Fast mode supports at most 512 MiB".into());
        }
        let disk = Disk::open(&vm.disk, !frozen).map_err(|e| format!("Cannot open disk: {e}"))?;
        let disk = if frozen { disk.frozen() } else { disk };
        let cd = if vm.cdrom {
            vm.iso
                .as_ref()
                .map(File::open)
                .transpose()
                .map_err(|e| format!("Cannot open CD: {e}"))?
        } else {
            None
        };
        let cd_bytes = cd
            .as_ref()
            .map(|f| f.metadata().map(|m| m.len()))
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or(0);
        if cd_bytes > u64::from(u32::MAX) || cd_bytes % 2048 != 0 {
            return Err("Invalid CD size".into());
        }
        let mut assets = HashMap::new();
        for (name, mime) in [
            ("vm.html", "text/html; charset=utf-8"),
            ("vm.mjs", "text/javascript"),
            ("vm-worker.mjs", "text/javascript"),
            ("wasm.mjs", "text/javascript"),
            ("jit.mjs", "text/javascript"),
            ("r86-jit.wasm", "application/wasm"),
        ] {
            let path = firmware.join("web").join(name);
            assets.insert(
                format!("app/{name}"),
                (
                    mime,
                    std::fs::read(&path)
                        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?,
                ),
            );
        }
        for (name, path) in [
            (
                "rom/0",
                vm.bios.clone().unwrap_or_else(|| firmware.join("bios.bin")),
            ),
            (
                "rom/1",
                vm.vga_bios
                    .clone()
                    .unwrap_or_else(|| firmware.join("vgabios-stdvga.bin")),
            ),
        ] {
            assets.insert(
                name.into(),
                (
                    "application/octet-stream",
                    std::fs::read(&path)
                        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?,
                ),
            );
        }
        let (width, height, _) = recommended.unwrap_or((0, 0, 16));
        let config = format!("{{\"engine\":\"jit\",\"memoryMiB\":{},\"diskSectors\":{},\"cdBytes\":{cd_bytes},\"cdFirst\":{},\"cdrom\":{},\"readOnly\":false,\"usbTablet\":{},\"recommended\":[{width},{height}],\"jitMiB\":128,\"jitThreshold\":256,\"frozen\":{frozen}}}",
            vm.memory, disk.sectors(), vm.boot_order == "cdrom", vm.cdrom, vm.usb_tablet).into_bytes();
        Ok(Self {
            disk,
            cd,
            cd_bytes,
            config,
            assets,
        })
    }

    fn http(&mut self, request: &ws::HttpRequest, prefix: &str) -> Response {
        let mut words = request
            .headers
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace();
        let method = words.next().unwrap_or("");
        let target = words.next().unwrap_or("");
        let Some(target) = target.strip_prefix(prefix) else {
            return Response::error("404 Not Found", "Not found");
        };
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if method == "GET" && query.is_empty() {
            if path == "config" {
                return Response::new("200 OK", "application/json", self.config.clone());
            }
            if let Some((mime, bytes)) = self.assets.get(path) {
                return Response::new("200 OK", mime, bytes.clone());
            }
        }
        let (drive, flush) = match path {
            "disk/0" => (0, false),
            "disk/1" => (1, false),
            "disk/0/flush" => (0, true),
            "disk/1/flush" => (1, true),
            _ => return Response::error("404 Not Found", "Not found"),
        };
        let size = if drive == 0 {
            self.disk.sectors() * 512
        } else {
            self.cd_bytes
        };
        let result: Result<Response, String> = (|| {
            if method == "POST" && flush && query.is_empty() && request.body.is_empty() {
                if drive == 0 {
                    self.disk.flush().map_err(|e| e.to_string())?;
                }
                return Ok(Response::new(
                    "204 No Content",
                    "application/octet-stream",
                    Vec::new(),
                ));
            }
            if method == "POST" && !flush {
                let offset = query.strip_prefix("o=").and_then(|s| s.parse::<u64>().ok());
                let Some(offset) = offset else {
                    return Ok(Response::error("400 Bad Request", "Expected byte offset o"));
                };
                let length = request.body.len() as u64;
                if drive != 0 {
                    return Err("CD is read-only".into());
                }
                if length == 0
                    || offset % 512 != 0
                    || length % 512 != 0
                    || offset.checked_add(length).is_none_or(|end| end > size)
                {
                    return Ok(Response::error("400 Bad Request", "Invalid sector write"));
                }
                self.disk
                    .write(offset / 512, &request.body)
                    .map_err(|e| e.to_string())?;
                return Ok(Response::new(
                    "204 No Content",
                    "application/octet-stream",
                    Vec::new(),
                ));
            }
            if method != "GET" || flush || !query.is_empty() {
                return Ok(Response::error(
                    "405 Method Not Allowed",
                    "Invalid media request",
                ));
            }
            let range = ws::unique_header(&request.headers, "range")?
                .and_then(|v| v.strip_prefix("bytes="))
                .and_then(|v| v.split_once('-'))
                .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)));
            let Some((start, end)) =
                range.filter(|(a, b)| a <= b && *b < size && b - a < MAX_BODY as u64)
            else {
                let mut response = Response::error(
                    "416 Range Not Satisfiable",
                    "Expected one bounded byte range",
                );
                response.headers =
                    format!("Content-Range: bytes */{size}\r\nAccept-Ranges: bytes\r\n");
                return Ok(response);
            };
            let count = (end - start + 1) as usize;
            let body = if drive == 0 {
                let skip = (start % 512) as usize;
                let mut aligned = vec![0; (skip + count).div_ceil(512) * 512];
                self.disk
                    .read(start / 512, &mut aligned)
                    .map_err(|e| e.to_string())?;
                aligned[skip..skip + count].to_vec()
            } else {
                let cd = self.cd.as_mut().ok_or("No CD inserted")?;
                cd.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
                let mut bytes = vec![0; count];
                cd.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                bytes
            };
            let mut response =
                Response::new("206 Partial Content", "application/octet-stream", body);
            response.headers =
                format!("Content-Range: bytes {start}-{end}/{size}\r\nAccept-Ranges: bytes\r\n");
            Ok(response)
        })();
        result.unwrap_or_else(|e| Response::error("500 Internal Server Error", e))
    }
}

struct Transport {
    stop: AtomicBool,
    claimed: AtomicBool,
    sockets: Mutex<HashMap<u64, TcpStream>>,
}

fn token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    #[cfg(unix)]
    // SAFETY: getentropy writes this 16-byte buffer (maximum is 256).
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err(format!(
            "Cannot generate web session token: {}",
            std::io::Error::last_os_error()
        ));
    }
    #[cfg(not(unix))]
    return Err("Web sessions require an Apple or Unix host".into());
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn connection(
    mut stream: TcpStream,
    host: &str,
    prefix: &str,
    transport: &Transport,
    events: &mpsc::SyncSender<Event>,
) -> Result<(), String> {
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    loop {
        // Idle expiry and peer close end a reused connection without an HTTP error.
        match stream.peek(&mut [0]) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Ok(());
            }
            Err(e) => return Err(e.to_string()),
        }
        let request = match ws::HttpRequest::read(&mut stream, MAX_BODY) {
            Ok(request) => request,
            Err(error) => {
                return ws::http_reply(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    error.as_bytes(),
                );
            }
        };
        let words: Vec<_> = request
            .headers
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .collect();
        let authorized = words.len() == 3
            && words[2] == "HTTP/1.1"
            && words[1].starts_with(prefix)
            && ws::unique_header(&request.headers, "host")? == Some(host);
        if !authorized {
            return ws::http_reply(&mut stream, "404 Not Found", "text/plain", b"Not found");
        }
        if words[1] == format!("{prefix}ctl") && words[0] == "GET" {
            if ws::unique_header(&request.headers, "origin")?
                != Some(format!("http://{host}").as_str())
                || transport.claimed.swap(true, Ordering::AcqRel)
            {
                return ws::http_reply(&mut stream, "404 Not Found", "text/plain", b"Not found");
            }
            let socket = ws::WebSocket::upgrade(stream, &request.headers)?;
            let mut reader = socket.reader()?;
            events
                .send(Event::Connected(socket))
                .map_err(|_| "Session stopped")?;
            while let Ok(message) = reader.read_message() {
                match message {
                    ws::Message::Text(text) => {
                        if events.send(Event::Text(text)).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            let _ = events.send(Event::Disconnected);
            return Ok(());
        }
        let keep_alive = !ws::unique_header(&request.headers, "connection")?.is_some_and(|value| {
            value
                .split(',')
                .any(|word| word.trim().eq_ignore_ascii_case("close"))
        });
        let (send, receive) = mpsc::sync_channel(1);
        events
            .send(Event::Http(request, send))
            .map_err(|_| "Session stopped")?;
        let response = receive.recv().map_err(|_| "Session stopped")?;
        ws::http_reply_headers(
            &mut stream,
            response.status,
            response.mime,
            &response.headers,
            &response.body,
            keep_alive,
        )?;
        if !keep_alive {
            return Ok(());
        }
    }
}

fn accept_connections(
    listener: TcpListener,
    host: String,
    prefix: String,
    transport: &Arc<Transport>,
    send: mpsc::SyncSender<Event>,
) -> std::io::Result<JoinHandle<()>> {
    let connections = transport.clone();
    std::thread::Builder::new()
        .name("r86 web accept".into())
        .spawn(move || {
            let mut id = 0u64;
            for stream in listener.incoming() {
                if connections.stop.load(Ordering::Acquire) {
                    break;
                }
                let stream = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = send.send(Event::Failed(e.to_string()));
                        break;
                    }
                };
                let mut sockets = lock(&connections.sockets);
                if sockets.len() >= 8 {
                    continue;
                }
                let copy = match stream.try_clone() {
                    Ok(copy) => copy,
                    Err(_) => continue,
                };
                id += 1;
                sockets.insert(id, copy);
                drop(sockets);
                let (connections, send, host, prefix) = (
                    connections.clone(),
                    send.clone(),
                    host.clone(),
                    prefix.clone(),
                );
                let failed = send.clone();
                if let Err(error) = std::thread::Builder::new()
                    .name("r86 web I/O".into())
                    .spawn(move || {
                        let _ = connection(stream, &host, &prefix, &connections, &send);
                        lock(&connections.sockets).remove(&id);
                    })
                {
                    let _ = failed.send(Event::Failed(format!(
                        "Cannot start web connection: {error}"
                    )));
                    break;
                }
            }
        })
}

pub fn start(
    vm: Vm,
    firmware: PathBuf,
    logs: PathBuf,
    recommended: Option<DisplayMode>,
    frozen: bool,
    shared: Arc<Shared>,
) -> Result<(Link, JoinHandle<()>), String> {
    let vm_lock = vm_store::lock_vm(&vm.directory)?;
    let mut media = Media::open(&vm, &firmware, recommended, frozen)?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let host = address.to_string();
    let prefix = format!("/{}/", token()?);
    let url = format!("http://{host}{prefix}app/vm.html");
    let (send, receive) = mpsc::sync_channel(64);
    let phase = Arc::new(AtomicU32::new(0));
    let report = Arc::new(Mutex::new(String::new()));
    let link = Link {
        url,
        state: phase.clone(),
        report: report.clone(),
        events: send.clone(),
    };
    let transport = Arc::new(Transport {
        stop: AtomicBool::new(false),
        claimed: AtomicBool::new(false),
        sockets: Mutex::default(),
    });
    let thread = std::thread::Builder::new()
        .name("r86 web disks".into())
        .spawn(move || {
            run_log::start(&logs, &vm);
            run_log::event(
                0,
                "web engine: Wasm JIT, CPU/devices in Worker, native disk owner",
            );
            let accept = accept_connections(listener, host, prefix.clone(), &transport, send);
            let mut socket = None;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Err(error) = &accept {
                    Err(format!("Cannot start web listener: {error}"))
                } else {
                    serve(
                        &mut media,
                        &prefix,
                        &receive,
                        &shared,
                        &phase,
                        &report,
                        &mut socket,
                    )
                }
            }));
            let mut error = match result {
                Ok(result) => result.err(),
                Err(_) => Some("r86 web host panicked".into()),
            };
            // Unblock transport senders before joining the accept thread.
            drop(receive);
            if let Some(socket) = &mut socket {
                let _ = socket.send_text("stop");
                socket.shutdown();
            }
            transport.stop.store(true, Ordering::Release);
            for socket in lock(&transport.sockets).values() {
                let _ = socket.shutdown(Shutdown::Both);
            }
            let _ = TcpStream::connect(address); // wake only this listener so it can close
            if let Ok(accept) = accept {
                let _ = accept.join();
            }
            if let Err(failure) = media.disk.flush() {
                error.get_or_insert_with(|| format!("Disk flush failed: {failure}"));
            }
            if error.is_some() && !lock(&report).is_empty() {
                run_log::event(0, &lock(&report));
            }
            let log = error.as_ref().and_then(|e| run_log::fatal(e, None, None));
            let mut status = lock(&shared.status);
            status.state = if error.is_some() {
                State::Failed
            } else {
                State::Off
            };
            status.ips = 0;
            status.log = log;
            status.messages.extend(error);
            drop(status);
            drop(media);
            drop(vm_lock);
            run_log::end();
        })
        .map_err(|e| e.to_string())?;
    Ok((link, thread))
}

fn command(command: Command) -> Option<String> {
    Some(match command {
        Command::Pause(paused) => format!("control {}", u8::from(!paused)),
        Command::Reset => "control 2".into(),
        Command::PowerButton => "control 3".into(),
        Command::CtrlAltDel => "control 4".into(),
        Command::ReleaseInput => "control 5".into(),
        Command::Key(key, down) => format!("key {} {}", key.wire_code(), u8::from(down)),
        Command::Mouse(dx, dy, buttons) => format!(
            "mouse {dx} {dy} {}",
            u8::from(buttons.left)
                | (u8::from(buttons.right) << 1)
                | (u8::from(buttons.middle) << 2)
        ),
        Command::Pointer(x, y, buttons, wheel) => format!("pointer {x} {y} {buttons} {wheel}"),
        _ => return None,
    })
}

fn serve(
    media: &mut Media,
    prefix: &str,
    receive: &mpsc::Receiver<Event>,
    shared: &Shared,
    phase: &AtomicU32,
    report: &Mutex<String>,
    socket: &mut Option<ws::WebSocket>,
) -> Result<(), String> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut pending = Vec::new();
    let mut unavailable = false;
    let mut probe = false;
    loop {
        let Some(commands) = shared.take_commands() else {
            return Ok(());
        };
        for action in commands {
            if matches!(action, Command::Cd(_) | Command::Remote(_)) && !unavailable {
                unavailable = true;
                lock(&shared.status)
                    .messages
                    .push_back("Not available in fast mode yet".into());
            }
            if let Some(command) = command(action) {
                pending.push(command);
            }
        }
        if phase.load(Ordering::Acquire) == 1
            && let Some(socket) = socket.as_mut()
        {
            for command in pending.drain(..) {
                socket.send_text(&command)?;
            }
        }
        let event = if phase.load(Ordering::Acquire) == 0 {
            match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    phase.store(2, Ordering::Release);
                    return Ok(());
                }
                Err(_) => return Err("Web event channel closed".into()),
            }
        } else {
            receive.recv().map_err(|_| "Web event channel closed")?
        };
        match event {
            Event::Wake => {}
            Event::Http(request, reply) => {
                let _ = reply.send(media.http(&request, prefix));
            }
            Event::Connected(connection) => *socket = Some(connection),
            Event::Disconnected => {
                if phase.load(Ordering::Acquire) == 0 {
                    phase.store(2, Ordering::Release);
                    return Ok(());
                }
                return Err(
                    "Fast mode's web-content process disconnected; the VM lost power.".into(),
                );
            }
            Event::Failed(error) => return Err(error),
            Event::Text(text) => {
                if text.starts_with("probe ") && (probe || phase.load(Ordering::Acquire) != 0) {
                    return Err("Unexpected web probe".into());
                }
                if text.starts_with("probe 0 ") {
                    phase.store(2, Ordering::Release);
                    run_log::event(0, &text);
                    return Ok(());
                }
                if let Some(ms) = text.strip_prefix("probe 1 ") {
                    if !ms
                        .parse::<f64>()
                        .is_ok_and(|ms| ms.is_finite() && (0.0..=500.0).contains(&ms))
                    {
                        return Err("Invalid web probe time".into());
                    }
                    probe = true;
                    run_log::event(0, &text);
                    continue;
                }
                if text == "ready" && probe && phase.load(Ordering::Acquire) == 0 {
                    phase.store(1, Ordering::Release);
                    continue;
                }
                if text == "off" {
                    return Ok(());
                }
                if let Some(error) = text.strip_prefix("error ") {
                    if phase.load(Ordering::Acquire) == 0 {
                        phase.store(2, Ordering::Release);
                        run_log::event(0, error);
                        return Ok(());
                    }
                    return Err(error.into());
                }
                if let Some(message) = text.strip_prefix("message ") {
                    run_log::event(0, message);
                    let mut status = lock(&shared.status);
                    if status.messages.len() < 16 {
                        status.messages.push_back(message.into());
                    }
                    continue;
                }
                let fields: Vec<_> = text.split_whitespace().collect();
                if fields.len() != 16 || fields[0] != "status" || phase.load(Ordering::Acquire) != 1
                {
                    return Err("Invalid web status message".into());
                }
                let values = fields[1..]
                    .iter()
                    .map(|v| v.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0))
                    .collect::<Option<Vec<_>>>()
                    .ok_or("Invalid web status number")?;
                if values[..7].iter().any(|n| n.fract() != 0.0)
                    || values[0] > 3.0
                    || values[3] > 4096.0
                    || values[4] > 4096.0
                    || values[5] > 1.0
                {
                    return Err("Invalid web status bounds".into());
                }
                let state = match values[0] as u32 {
                    0 => State::Running,
                    1 => State::Paused,
                    2 => return Ok(()),
                    _ => return Err("Web VM failed".into()),
                };
                {
                    let mut status = lock(&shared.status);
                    status.state = state;
                    status.ips = values[1] as u64;
                }
                shared.pointer.store(values[5] != 0.0, Ordering::Relaxed);
                let mut frame = lock(&shared.frame);
                let (width, height) = (values[3] as u32, values[4] as u32);
                if frame.width != width || frame.height != height {
                    frame.width = width;
                    frame.height = height;
                    frame.serial += 1;
                }
                *lock(report) = text;
            }
        }
    }
}
