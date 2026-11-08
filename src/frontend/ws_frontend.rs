//! Browser and debugger share one machine, owned by this thread. Network I/O
//! never runs here. Continuous execution and finite debugger jobs yield between
//! instruction slices; display conversion happens only on explicit polls.
//! Logs use caller-owned offsets, so readers never consume each other's data.
use super::disk_files;
use super::keys::{InputKey, inject_input_key, parse_named_key};
use super::media::{Image, ImageKind, MEDIA_LIMIT, Upload};
use super::protocol::*;
use super::ws;
use r86::devices::{Ps2ButtonState, Viewer};
use r86::machine::{GuestStepStatus, Machine};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

#[derive(Default)]
struct Reply {
    text: String,
    binary: Option<Arc<[u8]>>,
    close: bool,
}
impl Reply {
    fn text(text: String) -> Self {
        Self {
            text,
            ..Self::default()
        }
    }
}

struct WsSession {
    slice_steps: u64,
    serial_cursor: usize,
    mouse_buttons: Ps2ButtonState,
    record: Option<Arc<fs::File>>,
    held_keys: Vec<(InputKey, String)>,
}

impl WsSession {
    fn new(slice_steps: u64, record: Option<Arc<fs::File>>) -> Self {
        Self {
            slice_steps,
            serial_cursor: 0,
            mouse_buttons: Ps2ButtonState::default(),
            record,
            held_keys: Vec::new(),
        }
    }

    fn hello(&self, machine: &Machine) -> String {
        let response = format!(
            "{{\"type\":\"hello\",\"ok\":true,\"version\":1,\"step\":{},\"slice_steps\":{},\"capabilities\":[\"run-steps\",\"run-until-step\",\"wait-text\",\"wait-serial\",\"type\",\"keydown\",\"keyup\",\"send-scancodes\",\"mouse-move\",\"mouse-button\",\"power-button\",\"text-snapshot\",\"status\",\"hash-frame\",\"screenshot\",\"wait-frame-stable\",\"resume\",\"pause\",\"reset\",\"frame\",\"logs\",\"diagnostics\",\"memory\",\"release-input\",\"guest-bytes\",\"media\",\"power-on\",\"power-off\",\"files\",\"quit\"]}}",
            machine.step_count(),
            self.slice_steps
        );
        response
    }

    fn handle_text(
        &mut self,
        reply: &mut Reply,
        machine: &mut Machine,
        cmd: &str,
        message: &str,
    ) -> Result<bool, String> {
        match cmd {
            "power-button" => {
                machine.press_power_button();
                self.record_line(&format!(
                    "{{\"step\":{},\"cmd\":\"power-button\"}}",
                    machine.step_count()
                ))?;
                reply.text = format!(
                    "{{\"type\":\"ack\",\"ok\":true,\"cmd\":\"power-button\",\"step\":{},\"applied_step\":{}}}",
                    machine.step_count(),
                    machine.step_count()
                );
            }
            "guest-bytes" => reply.text = guest_bytes_json(machine, true),
            "send-scancodes" => {
                let hex = json_string(message, "hex")?
                    .ok_or_else(|| "send-scancodes missing hex".to_string())?;
                let bytes = parse_hex_bytes(&hex)?;
                machine.inject_scancode_set2(&bytes);

                self.record_line(&format!(
                    "{{\"step\":{},\"cmd\":\"send-scancodes\",\"hex\":\"{}\"}}",
                    machine.step_count(),
                    json_escape(&hex)
                ))?;
                reply.text = format!(
                    "{{\"type\":\"ack\",\"ok\":true,\"cmd\":\"send-scancodes\",\"step\":{},\"applied_step\":{},\"bytes\":{}}}",
                    machine.step_count(),
                    machine.step_count(),
                    bytes.len()
                );
            }
            "text-snapshot" => {
                reply.text = text_snapshot_json(machine, true);
            }
            "hash-frame" => {
                let frame = capture_frame(machine)?;
                reply.text = frame_hash_json(machine.step_count(), &frame, true);
            }
            "screenshot" => {
                let frame = capture_frame(machine)?;
                reply.text = frame_hash_json(machine.step_count(), &frame, true);
                let binary = ws::encode_rgba_frame(
                    machine.step_count(),
                    frame.width,
                    frame.height,
                    frame.stride,
                    &frame.pixels,
                );
                reply.binary = Some(binary.into());
            }
            "quit" => {
                reply.text = format!(
                    "{{\"type\":\"bye\",\"ok\":true,\"step\":{}}}",
                    machine.step_count()
                );
                return Ok(false);
            }
            other => return Err(format!("unknown command {other}")),
        }
        Ok(true)
    }

    fn take_serial_hex(&mut self, machine: &Machine) -> String {
        let start = self.serial_cursor.min(machine.serial_output().len());
        let hex = hex_encode(&machine.serial_output()[start..]);
        self.serial_cursor = machine.serial_output().len();
        hex
    }

    fn record_line(&mut self, line: &str) -> Result<(), String> {
        record_line(self.record.as_deref(), line)
    }
}

fn record_line(file: Option<&fs::File>, line: &str) -> Result<(), String> {
    if let Some(mut file) = file {
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.flush())
            .map_err(|err| format!("failed to write websocket record: {err}"))?;
    }
    Ok(())
}

struct Client {
    session: WsSession,
    reply: mpsc::SyncSender<Reply>,
}
enum Request {
    Connect(usize, mpsc::SyncSender<Reply>),
    Command(usize, String),
    /// A binary frame: the next chunk of this client's media upload.
    Data(usize, Vec<u8>),
    Disconnect(usize),
}

const TEXT_QUEUE_LIMIT: usize = 4096;

// Accepted text is a discrete command, unlike a held physical key: it survives
// the sender disconnecting, so the one-shot CLI can enqueue while paused.
// Keep whole commands in order and at most TEXT_QUEUE_LIMIT remaining bytes.
struct PendingText {
    client: usize,
    text: String,
    offset: usize,
}

struct Service {
    clients: BTreeMap<usize, Client>,
    running: bool,
    job: Option<Job>,
    last_stop: String,
    failures: VecDeque<String>,
    last_shutdown_step: Option<u64>,
    events: VecDeque<(u64, String)>,
    event_next: u64,
    frame: Option<Arc<[u8]>>,
    frame_id: u64,
    text_queue: VecDeque<PendingText>,
    /// At most one virtual-media upload at a time, across all clients.
    upload: Option<Upload>,
    /// Name of the disc inserted through this server, if any.
    media_name: Option<String>,
    /// Where uploaded discs are saved (the handheld VM's tmp folder); None keeps
    /// them in memory.
    save_dir: Option<PathBuf>,
    /// A disc this server inserted or ejected, until the owner takes it.
    disc_change: Option<Option<PathBuf>>,
    /// A file being uploaded into the powered-off VM's disk.
    file_put: Option<FilePut>,
    /// Finite debugger jobs are available (r86-run's own loop steps them).
    jobs: bool,
}

/// A file being copied onto the powered-off VM's disk.
struct FilePut {
    owner: usize,
    path: String,
    size: usize,
    received: usize,
    /// Received bytes not yet written, at most FILE_BATCH plus one chunk.
    pending: Vec<u8>,
}

/// Bytes a copy gathers before writing them: most files reach the guest disk
/// in one pass, and bigger ones reopen the file once per batch, not per chunk.
const FILE_BATCH: usize = 8 << 20;

fn bus_diagnostics_json(machine: &Machine) -> String {
    let limit = r86::machine::BUS_PAGE_LOG_LIMIT;
    let pages = machine.bus_warning_pages().map(|p| format!(
        "{{\"space\":\"{}\",\"page\":{},\"count\":{},\"suppressed\":{},\"muted\":{},\"last_address\":{},\"last_write\":{},\"last_value\":{},\"last_step\":{}}}",
        p.space.name(), p.address & !4095, p.count, p.count - p.logged,
        p.count >= limit, p.address, p.write, p.value, p.step
    )).collect::<Vec<_>>().join(",");
    let sources = machine
        .bus_warning_instruction_pages()
        .map(|(page, p)| {
            format!(
                "{{\"page\":{page},\"count\":{},\"suppressed\":{},\"muted\":{}}}",
                p.count,
                p.count - p.logged,
                p.count >= limit
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"page_size\":4096,\"log_limit\":{limit},\"pages\":[{pages}],\"instruction_pages\":[{sources}]}}"
    )
}

fn cpu_shutdown_json(shutdown: &r86::machine::CpuShutdown) -> String {
    format!(
        "{{\"step\":{},\"execution\":{{\"running\":false,\"stop\":{{\"kind\":\"triple-fault\",\"message\":\"{}\"}}}},\"reset\":true,\"cpu_dump\":\"{}\",\"vga_dump\":\"{}\",\"serial_hex\":\"{}\"}}",
        shutdown.step,
        json_escape(&format!("{:?}", shutdown.error)),
        json_escape(&shutdown.cpu_dump),
        json_escape(&shutdown.vga_dump),
        hex_encode(&shutdown.serial),
    )
}

impl Service {
    fn new() -> Self {
        Self {
            clients: BTreeMap::new(),
            running: false,
            job: None,
            last_stop: "null".into(),
            failures: VecDeque::new(),
            last_shutdown_step: None,
            events: VecDeque::new(),
            event_next: 0,
            frame: None,
            frame_id: 0,
            text_queue: VecDeque::new(),
            upload: None,
            media_name: None,
            save_dir: None,
            disc_change: None,
            file_put: None,
            jobs: false,
        }
    }
    fn pending_text_bytes(&self) -> usize {
        self.text_queue
            .iter()
            .map(|pending| pending.text.len() - pending.offset)
            .sum()
    }
    fn queue_text(&mut self, client: usize, text: String) -> Result<(), String> {
        if !text
            .bytes()
            .all(|b| matches!(b, b'\t' | b'\n' | b'\r' | b' '..=b'~'))
        {
            return Err("type supports printable US ASCII, tab and newline only".into());
        }
        if text.len() > TEXT_QUEUE_LIMIT - self.pending_text_bytes() {
            return Err(format!(
                "type queue is full (limit {TEXT_QUEUE_LIMIT} bytes)"
            ));
        }
        if !text.is_empty() {
            self.text_queue.push_back(PendingText {
                client,
                text,
                offset: 0,
            });
        }
        Ok(())
    }
    fn cancel_text(&mut self, machine: &Machine, client: Option<usize>, reason: &str) {
        let before = self.pending_text_bytes();
        self.text_queue
            .retain(|pending| client.is_some_and(|id| pending.client != id));
        let cancelled = before - self.pending_text_bytes();
        if cancelled != 0 {
            self.event(
                machine,
                "input-cancelled",
                &format!("{cancelled} text bytes: {reason}"),
            );
        }
    }
    fn deliver_text(
        &mut self,
        machine: &mut Machine,
        record: Option<&fs::File>,
    ) -> Result<(), String> {
        if self.text_queue.is_empty()
            || self
                .clients
                .values()
                .any(|c| !c.session.held_keys.is_empty())
            || !machine.keyboard_ready_for_text()
        {
            return Ok(());
        }
        let pending = self.text_queue.front_mut().unwrap();
        let ch = &pending.text[pending.offset..pending.offset + 1];
        // Record delivery, not admission. Replay therefore sees the same device
        // state at the same guest step, including pauses and queue backpressure.
        if record.is_some() {
            record_line(
                record,
                &format!(
                    "{{\"step\":{},\"cmd\":\"type\",\"text\":\"{}\"}}",
                    machine.step_count(),
                    json_escape(ch)
                ),
            )?;
        }
        machine.inject_ascii(ch.as_bytes());
        pending.offset += 1;
        if pending.offset == pending.text.len() {
            self.text_queue.pop_front();
        }
        Ok(())
    }
    fn event(&mut self, machine: &Machine, kind: &str, message: &str) {
        self.event_at(machine.step_count(), kind, message);
    }
    fn event_at(&mut self, step: u64, kind: &str, message: &str) {
        let seq = self.event_next;
        self.event_next += 1;
        self.events.push_back((
            seq,
            format!(
                "{{\"seq\":{seq},\"step\":{},\"kind\":\"{}\",\"message\":\"{}\"}}",
                step,
                json_escape(kind),
                json_escape(message)
            ),
        ));
        if self.events.len() > 256 {
            self.events.pop_front();
        }
    }
    fn collect_bus_warnings(&mut self, machine: &mut Machine) {
        while let Some(warning) = machine.take_bus_warning() {
            self.event_at(
                warning.step,
                "unmapped-bus",
                &format!(
                    "{} {} address={:#x} value={:#04x} page={:#x} count={} instruction={} instruction_count={}{}",
                    warning.space.name(),
                    if warning.write {
                        "write ignored"
                    } else {
                        "floating read"
                    },
                    warning.address,
                    warning.value,
                    warning.address & !4095,
                    warning.count,
                    warning.instruction.map_or_else(|| "none".into(), |pc| format!("{pc:#x}")),
                    warning.instruction_count,
                    if warning.count == r86::machine::BUS_PAGE_LOG_LIMIT
                        || warning.instruction_count == r86::machine::BUS_PAGE_LOG_LIMIT {
                        "; further logs muted by page limit"
                    } else {
                        ""
                    }
                ),
            );
        }
    }
    fn collect_cpu_shutdown(&mut self, machine: &Machine) {
        let Some(shutdown) = machine.last_cpu_shutdown() else {
            return;
        };
        if self.last_shutdown_step == Some(shutdown.step) {
            return;
        }
        self.last_shutdown_step = Some(shutdown.step);
        self.event_at(
            shutdown.step,
            "cpu-shutdown",
            &format!("{:?}; board reset", shutdown.error),
        );
        // Record the pre-reset CPU, not the already rebooted machine. A CPU
        // shutdown resets the board; it does not stop the WS execution job.
        self.failures.push_back(cpu_shutdown_json(shutdown));
        if self.failures.len() > 8 {
            self.failures.pop_front();
        }
    }
    fn state(&self, machine: &Machine) -> String {
        format!(
            "{{\"type\":\"execution\",\"ok\":true,\"step\":{},\"running\":{},\"busy\":{},\"stop\":{},\"pending_text_bytes\":{}}}",
            machine.step_count(),
            self.running,
            self.job.is_some(),
            self.last_stop,
            self.pending_text_bytes()
        )
    }
    fn snapshot(&self, machine: &Machine) -> String {
        let serial = machine.serial_output();
        format!(
            "{{\"status\":{},\"execution\":{},\"cpu_dump\":\"{}\",\"vga_dump\":\"{}\",\"text\":\"{}\",\"serial_hex\":\"{}\",\"trace\":\"{}\",\"jit\":\"{}\",\"bus\":{},\"cpu_shutdown\":{}}}",
            status_json(machine, true),
            self.state(machine),
            json_escape(&format!("{:#?}", machine.cpu())),
            json_escape(&machine.vga().register_report()),
            json_escape(&machine.text_snapshot()),
            hex_encode(&serial[serial.len().saturating_sub(16384)..]),
            json_escape(&machine.trace_report()),
            json_escape(&machine.jit_report()),
            bus_diagnostics_json(machine),
            machine
                .last_cpu_shutdown()
                .map(cpu_shutdown_json)
                .unwrap_or_else(|| "null".into()),
        )
    }
    fn stop(&mut self, machine: &mut Machine, stop: String, failure: bool) {
        self.running = false;
        self.cancel_text(machine, None, "execution stopped");
        self.last_stop = stop;
        self.event(
            machine,
            if failure { "crash" } else { "stop" },
            &self.last_stop.clone(),
        );
        if failure {
            let snapshot = self.snapshot(machine);
            self.failures.push_back(snapshot);
            if self.failures.len() > 8 {
                self.failures.pop_front();
            }
        }
    }
    fn failure(&mut self, machine: &mut Machine, error: &str) {
        self.stop(
            machine,
            format!(
                "{{\"kind\":\"error\",\"message\":\"{}\"}}",
                json_escape(error)
            ),
            true,
        );
    }
    fn reply(&mut self, id: usize, reply: Reply) {
        // One outstanding request per worker. No VM thread waits for TCP output.
        if let Some(client) = self.clients.get(&id) {
            let _ = client.reply.try_send(reply);
        }
    }
    fn buttons(&self) -> Ps2ButtonState {
        self.clients
            .values()
            .fold(Ps2ButtonState::default(), |mut b, c| {
                b.left |= c.session.mouse_buttons.left;
                b.right |= c.session.mouse_buttons.right;
                b.middle |= c.session.mouse_buttons.middle;
                b
            })
    }
    fn key(
        &mut self,
        machine: &mut Machine,
        id: usize,
        name: &str,
        down: bool,
    ) -> Result<(), String> {
        let key = parse_named_key(name)?;
        let before = self
            .clients
            .values()
            .any(|c| c.session.held_keys.iter().any(|(k, _)| *k == key));
        let session = &mut self.clients.get_mut(&id).unwrap().session;
        let repeat = down && session.held_keys.iter().any(|(k, _)| *k == key);
        if down {
            if !session.held_keys.iter().any(|(k, _)| *k == key) {
                session.held_keys.push((key, name.into()));
            }
        } else {
            session.held_keys.retain(|(k, _)| *k != key);
        }
        let after = self
            .clients
            .values()
            .any(|c| c.session.held_keys.iter().any(|(k, _)| *k == key));
        if before != after || repeat {
            inject_input_key(machine, key, after);
            self.clients
                .get_mut(&id)
                .unwrap()
                .session
                .record_line(&format!(
                    "{{\"step\":{},\"cmd\":\"{}\",\"key\":\"{}\"}}",
                    machine.step_count(),
                    if after { "keydown" } else { "keyup" },
                    json_escape(name)
                ))?;
        }
        Ok(())
    }
    fn button(
        &mut self,
        machine: &mut Machine,
        id: usize,
        name: &str,
        down: bool,
    ) -> Result<(), String> {
        let before = self.buttons();
        set_mouse_button(
            &mut self.clients.get_mut(&id).unwrap().session.mouse_buttons,
            name,
            down,
        )?;
        let after = self.buttons();
        if before != after {
            machine.inject_mouse(0, 0, after);
            self.clients
                .get_mut(&id)
                .unwrap()
                .session
                .record_line(&format!(
                    "{{\"step\":{},\"cmd\":\"mouse-button\",\"button\":\"{}\",\"down\":{down}}}",
                    machine.step_count(),
                    json_escape(name)
                ))?;
        }
        Ok(())
    }
    fn release_input(&mut self, machine: &mut Machine, id: usize) -> Result<(), String> {
        let Some(client) = self.clients.get(&id) else {
            return Ok(());
        };
        let keys = client.session.held_keys.clone();
        for (_, name) in keys {
            self.key(machine, id, &name, false)?;
        }
        for name in ["left", "right", "middle"] {
            self.button(machine, id, name, false)?;
        }
        Ok(())
    }
    fn cancel_job(&mut self, machine: &Machine, reason: &str) {
        if let Some(job) = self.job.take() {
            self.reply(
                job.client,
                Reply::text(error_json(machine.step_count(), reason)),
            );
        }
    }
    fn command(
        &mut self,
        machine: &mut Machine,
        id: usize,
        message: &str,
    ) -> Result<Option<Reply>, String> {
        let cmd = json_string(message, "cmd")?.ok_or("missing cmd")?;
        if Job::is_command(&cmd) {
            if !self.jobs {
                return Err(format!("{cmd} is not available on this device"));
            }
            if self.running || self.job.is_some() {
                return Err("execution is busy; pause before stepping or waiting".into());
            }
            self.job = Some(Job::new(
                id,
                &cmd,
                message,
                machine,
                &self.clients[&id].session,
            )?);
            return Ok(None);
        }
        let text = match cmd.as_str() {
            "hello" => {
                let mut hello = self.clients[&id].session.hello(machine);
                hello.pop();
                format!(
                    "{hello},\"client_id\":{id},\"execution\":{}}}",
                    self.state(machine)
                )
            }

            "resume" => {
                if self.job.is_some() {
                    return Err("execution is busy; pause first".into());
                }
                if machine.powered_off() {
                    return Err("guest is powered off; reset to start it".into());
                }
                self.running = true;
                self.last_stop = "null".into();
                self.event(machine, "resume", "continuous execution");
                self.state(machine)
            }
            "pause" => {
                self.running = false;
                self.cancel_job(machine, "cancelled by pause");
                if let Err(error) = machine.flush_disks() {
                    self.failure(machine, &error);
                    return Err(error);
                }
                self.event(machine, "pause", "execution paused");
                self.state(machine)
            }
            "reset" => {
                self.running = false;
                self.cancel_job(machine, "cancelled by reset");
                self.cancel_text(machine, None, "reset");
                for client in self.clients.keys().copied().collect::<Vec<_>>() {
                    self.release_input(machine, client)?;
                }
                if let Err(error) = machine.reboot() {
                    self.failure(machine, &error);
                    return Err(error);
                }
                self.frame = None;
                self.last_stop = "null".into();
                self.clients
                    .get_mut(&id)
                    .unwrap()
                    .session
                    .record_line(&format!(
                        "{{\"step\":{},\"cmd\":\"reset\"}}",
                        machine.step_count()
                    ))?;
                self.event(machine, "reset", "guest reset; paused");
                self.state(machine)
            }
            "type" => {
                if machine.powered_off() {
                    return Err("guest is powered off; reset to start it".into());
                }
                let text = json_string(message, "text")?.ok_or("type missing text")?;
                let bytes = text.len();
                self.queue_text(id, text)?;
                format!(
                    "{{\"type\":\"ack\",\"ok\":true,\"cmd\":\"type\",\"step\":{},\"queued_bytes\":{bytes},\"pending_text_bytes\":{}}}",
                    machine.step_count(),
                    self.pending_text_bytes()
                )
            }
            "keydown" | "keyup" => {
                let name = json_string(message, "key")?.ok_or("key event missing key")?;
                self.key(machine, id, &name, cmd == "keydown")?;
                format!(
                    "{{\"type\":\"ack\",\"ok\":true,\"cmd\":\"{cmd}\",\"key\":\"{}\",\"step\":{},\"applied_step\":{}}}",
                    json_escape(&name),
                    machine.step_count(),
                    machine.step_count()
                )
            }
            "mouse-button" => {
                let name = json_string(message, "button")?.ok_or("mouse-button missing button")?;
                let down = json_bool(message, "down")?.unwrap_or(false);
                self.button(machine, id, &name, down)?;
                format!(
                    "{{\"type\":\"ack\",\"ok\":true,\"cmd\":\"mouse-button\",\"button\":\"{}\",\"down\":{down},\"step\":{},\"applied_step\":{}}}",
                    json_escape(&name),
                    machine.step_count(),
                    machine.step_count()
                )
            }
            "mouse-move" => {
                let dx = clamp_i64_to_i16(json_i64(message, "dx")?.unwrap_or(0));
                let dy = clamp_i64_to_i16(json_i64(message, "dy")?.unwrap_or(0));
                machine.inject_mouse(dx, dy, self.buttons());
                self.clients
                    .get_mut(&id)
                    .unwrap()
                    .session
                    .record_line(&format!(
                        "{{\"step\":{},\"cmd\":\"mouse-move\",\"dx\":{dx},\"dy\":{dy}}}",
                        machine.step_count()
                    ))?;
                format!(
                    "{{\"type\":\"ack\",\"ok\":true,\"cmd\":\"mouse-move\",\"dx\":{dx},\"dy\":{dy},\"step\":{},\"applied_step\":{}}}",
                    machine.step_count(),
                    machine.step_count()
                )
            }
            "release-input" => {
                self.cancel_text(machine, Some(id), "release-input");
                self.release_input(machine, id)?;
                self.state(machine)
            }
            "status" => {
                let mut json = status_json(machine, true);
                json.pop();
                format!("{json},\"execution\":{}}}", self.state(machine))
            }
            "diagnostics" => {
                let current = self.snapshot(machine);
                format!(
                    "{{\"type\":\"diagnostics\",\"ok\":true,\"step\":{},\"current\":{},\"failures\":[{}]}}",
                    machine.step_count(),
                    current,
                    self.failures.iter().cloned().collect::<Vec<_>>().join(",")
                )
            }
            "logs" => {
                let serial = machine.serial_output();
                let from = json_u64(message, "serial_from")?
                    .unwrap_or(0)
                    .min(serial.len() as u64) as usize;
                let limit = json_u64(message, "limit")?.unwrap_or(8192).clamp(1, 65536) as usize;
                let end = from.saturating_add(limit).min(serial.len());
                let after = json_u64(message, "event_from")?.unwrap_or(0);
                let events = self
                    .events
                    .iter()
                    .filter(|(seq, _)| *seq >= after)
                    .map(|(_, s)| s.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{{\"type\":\"logs\",\"ok\":true,\"step\":{},\"serial_from\":{from},\"serial_next\":{end},\"serial_len\":{},\"serial_hex\":\"{}\",\"event_first\":{},\"event_next\":{},\"events\":[{events}]}}",
                    machine.step_count(),
                    serial.len(),
                    hex_encode(&serial[from..end]),
                    self.events.front().map_or(self.event_next, |(n, _)| *n),
                    self.event_next
                )
            }
            "memory" => {
                let address =
                    json_u64(message, "address")?.ok_or("memory requires physical RAM address")?;
                let length = json_u64(message, "length")?.unwrap_or(256);
                if length > 65536 {
                    return Err("memory length exceeds 65536".into());
                }
                let end = address.checked_add(length).ok_or("memory range overflow")?;
                if end > machine.ram().len() as u64 {
                    return Err("memory range is outside RAM".into());
                }
                format!(
                    "{{\"type\":\"memory\",\"ok\":true,\"step\":{},\"address\":{address},\"hex\":\"{}\"}}",
                    machine.step_count(),
                    hex_encode(&machine.ram()[address as usize..end as usize])
                )
            }
            "media" | "media-begin" | "media-file" | "media-dir" | "media-cancel"
            | "media-insert" | "media-eject" => self.media(machine, id, &cmd, message)?,
            "power-on" => {
                self.power_on(machine)?;
                self.clients
                    .get_mut(&id)
                    .unwrap()
                    .session
                    .record_line(&format!(
                        "{{\"step\":{},\"cmd\":\"power-on\"}}",
                        machine.step_count()
                    ))?;
                self.state(machine)
            }
            "power-off" => {
                self.power_off(machine)?;
                self.clients
                    .get_mut(&id)
                    .unwrap()
                    .session
                    .record_line(&format!(
                        "{{\"step\":{},\"cmd\":\"power-off\"}}",
                        machine.step_count()
                    ))?;
                self.state(machine)
            }
            "files-drives" | "files-list" | "files-get" | "files-put" | "files-mkdir"
            | "files-delete" | "files-rename" => {
                return self.files(machine, id, &cmd, message).map(Some);
            }
            "frame" => {
                let since = json_u64(message, "since")?;
                if self.frame.is_none() || machine.vga().frame_dirty_for(Viewer::Remote) {
                    let info = machine.vga().mode_info();
                    let width = u32::from(info.width);
                    let height = u32::from(info.height);
                    let mut pixels = vec![0; width as usize * height as usize * 4];
                    machine
                        .vga()
                        .render_rgba(&mut pixels)
                        .ok_or("VGA render failed")?;
                    self.frame = Some(
                        ws::encode_rgba_frame(
                            machine.step_count(),
                            width,
                            height,
                            width * 4,
                            &pixels,
                        )
                        .into(),
                    );
                    self.frame_id += 1;
                    machine.clear_frame_dirty_for(Viewer::Remote);
                }
                let changed = since != Some(self.frame_id);
                return Ok(Some(Reply {
                    text: format!(
                        "{{\"type\":\"frame\",\"ok\":true,\"step\":{},\"generation\":{},\"changed\":{changed}}}",
                        machine.step_count(),
                        self.frame_id
                    ),
                    binary: if changed { self.frame.clone() } else { None },
                    close: false,
                }));
            }
            _ => {
                let mut reply = Reply::default();
                reply.close = !self
                    .clients
                    .get_mut(&id)
                    .unwrap()
                    .session
                    .handle_text(&mut reply, machine, &cmd, message)?;
                return Ok(Some(reply));
            }
        };
        Ok(Some(Reply::text(text)))
    }
}

impl Service {
    /// Records what one guest slice did: bus warnings, CPU shutdowns, guest
    /// resets and stops (power-off, crashes, emulator errors).
    fn after_run(
        &mut self,
        machine: &mut Machine,
        result: &Result<r86::machine::RunReport, String>,
        reset_count: u64,
    ) {
        self.collect_bus_warnings(machine);
        self.collect_cpu_shutdown(machine);
        match result {
            Ok(report) => {
                if machine.reset_count() != reset_count {
                    self.cancel_text(machine, None, "guest reset");
                    self.frame = None;
                    self.event(machine, "reset", "guest requested reset");
                }
                if let Some(stop) = &report.stop
                    && !matches!(stop, GuestStepStatus::Halted)
                {
                    let failure = matches!(stop, GuestStepStatus::Stopped { .. });
                    self.stop(machine, stop_json(&report.stop), failure);
                }
            }
            Err(error) => self.stop(
                machine,
                format!(
                    "{{\"kind\":\"error\",\"message\":\"{}\"}}",
                    json_escape(error)
                ),
                true,
            ),
        }
    }

    /// Virtual media: status, uploads, insert and eject (see script/ws_cli.py).
    fn media(
        &mut self,
        machine: &mut Machine,
        id: usize,
        cmd: &str,
        message: &str,
    ) -> Result<String, String> {
        let mine = self
            .upload
            .as_ref()
            .is_some_and(|upload| upload.owner == id);
        match cmd {
            "media" => {}
            "media-begin" => {
                if self.upload.is_some() && !mine {
                    return Err("Another client is uploading media.".into());
                }
                let name = json_string(message, "name")?.unwrap_or_default();
                self.upload = Some(match json_string(message, "kind")?.as_deref() {
                    Some("iso") => {
                        let size = json_u64(message, "size")?.ok_or("media-begin missing size")?;
                        Upload::iso(id, &name, size, self.save_dir.as_deref())?
                    }
                    Some("files") => Upload::files(id, &name),
                    _ => return Err("media-begin kind must be iso or files".into()),
                });
            }
            "media-file" | "media-dir" => {
                let upload = self
                    .upload
                    .as_mut()
                    .filter(|_| mine)
                    .ok_or("No media upload in progress.")?;
                let path = json_string(message, "path")?.ok_or("missing path")?;
                if cmd == "media-dir" {
                    upload.add_dir(&path)?;
                } else {
                    let size = json_u64(message, "size")?.ok_or("media-file missing size")?;
                    upload.begin_file(&path, size)?;
                }
            }
            "media-cancel" => {
                if mine {
                    self.upload = None;
                }
            }
            "media-insert" => {
                if !mine {
                    return Err("No media upload in progress.".into());
                }
                // Check first so a refused change keeps the upload for a retry.
                machine.cdrom_change_allowed()?;
                let upload = self.upload.take().unwrap();
                let name = upload.name.clone();
                let (bytes, path) = match upload.finish(self.save_dir.as_deref())? {
                    Image::Bytes(data) => {
                        let bytes = data.len();
                        machine.change_cdrom(data)?;
                        (bytes, None)
                    }
                    Image::File(file, path) => {
                        let bytes = file.metadata().map_or(0, |m| m.len() as usize);
                        machine.change_file_cdrom(file)?;
                        (bytes, Some(path))
                    }
                };
                self.disc_change = Some(path);
                self.media_name = Some(name.clone());
                self.media_event(machine, id, "media-insert", &name, bytes)?;
            }
            "media-eject" => {
                machine.change_cdrom(Vec::new())?;
                self.disc_change = Some(None);
                self.media_name = None;
                self.media_event(machine, id, "media-eject", "", 0)?;
            }
            _ => unreachable!("dispatched media command"),
        }
        Ok(self.media_json(machine, id))
    }

    /// Files on the powered-off VM's hard disk (see script/ws_cli.py).
    fn files(
        &mut self,
        machine: &mut Machine,
        id: usize,
        cmd: &str,
        message: &str,
    ) -> Result<Reply, String> {
        if !machine.powered_off() {
            return Err("Power the VM off to change its disk; a running guest caches it.".into());
        }
        let disk = machine.hdd_mut().ok_or("This VM has no hard disk.")?;
        let path = json_string(message, "path")?.unwrap_or_default();
        let ack = |extra: &str| Reply::text(format!("{{\"type\":\"{cmd}\",\"ok\":true{extra}}}"));
        Ok(match cmd {
            "files-drives" => ack(&format!(",\"drives\":{}", disk_files::drives(disk))),
            "files-list" => ack(&format!(
                ",\"path\":\"{}\",\"items\":{}",
                json_escape(&path),
                disk_files::list(disk, &path)?
            )),
            "files-get" => {
                let data = disk_files::read(disk, &path)?;
                let mut reply = ack(&format!(",\"size\":{},\"binary\":true", data.len()));
                reply.binary = Some(data.into());
                reply
            }
            "files-put" => {
                let size = json_u64(message, "size")?.ok_or("files-put missing size")?;
                if size > MEDIA_LIMIT {
                    return Err(format!("Files are limited to {} MiB.", MEDIA_LIMIT >> 20));
                }
                if self.file_put.as_ref().is_some_and(|put| put.owner != id) {
                    return Err("Another client is uploading a file.".into());
                }
                // A replaced copy of this client leaves no partial file.
                if let Some(old) = self.file_put.take() {
                    let _ = disk_files::delete(disk, &old.path);
                }
                // The file is written in batches as it arrives, never held
                // whole in host memory (the handheld has little).
                disk_files::create(disk, &path, size)?;
                if size == 0 {
                    disk.flush().map_err(|err| err.to_string())?;
                    return Ok(Reply::text(
                        "{\"type\":\"files-data\",\"ok\":true,\"received\":0,\"saved\":true}"
                            .into(),
                    ));
                }
                let size = size as usize;
                self.file_put = Some(FilePut {
                    owner: id,
                    path,
                    size,
                    received: 0,
                    pending: Vec::with_capacity(
                        size.min(FILE_BATCH + ws::MAX_BINARY_FRAME as usize),
                    ),
                });
                ack("")
            }
            "files-mkdir" => {
                disk_files::mkdir(disk, &path)?;
                ack("")
            }
            "files-delete" => {
                disk_files::delete(disk, &path)?;
                ack("")
            }
            "files-rename" => {
                let to = json_string(message, "to")?.ok_or("files-rename missing to")?;
                disk_files::rename(disk, &path, &to)?;
                ack("")
            }
            _ => unreachable!("dispatched files command"),
        })
    }

    /// Appends upload data; the file is written when the last byte arrives.
    fn file_data(&mut self, machine: &mut Machine, data: &[u8]) -> Result<String, String> {
        let mut put = self.file_put.take().unwrap();
        if !machine.powered_off() {
            return Err("The VM was powered on; the file was not saved.".into());
        }
        let disk = machine.hdd_mut().ok_or("This VM has no hard disk.")?;
        put.received += data.len();
        put.pending.extend_from_slice(data);
        let received = put.received;
        let written = if received > put.size {
            Err("More data than the announced file size.".to_string())
        } else if received == put.size || put.pending.len() >= FILE_BATCH {
            let written = disk_files::append(disk, &put.path, &put.pending);
            put.pending.clear();
            written
        } else {
            Ok(())
        };
        if let Err(error) = written {
            let _ = disk_files::delete(disk, &put.path);
            return Err(error);
        }
        if received < put.size {
            self.file_put = Some(put);
            return Ok(format!(
                "{{\"type\":\"files-data\",\"ok\":true,\"received\":{received}}}"
            ));
        }
        disk.flush().map_err(|err| err.to_string())?;
        Ok(format!(
            "{{\"type\":\"files-data\",\"ok\":true,\"received\":{received},\"saved\":true}}"
        ))
    }

    fn power_on(&mut self, machine: &mut Machine) -> Result<(), String> {
        if !machine.powered_off() {
            return Err("The VM is already on.".into());
        }
        // A copy cut short by power-on leaves no partial file in the guest.
        if let Some(put) = self.file_put.take()
            && let Some(disk) = machine.hdd_mut()
        {
            let _ = disk_files::delete(disk, &put.path);
        }
        machine.reboot()?;
        self.frame = None;
        self.last_stop = "null".into();
        self.running = true;
        self.event(machine, "power-on", "powered on");
        Ok(())
    }

    fn power_off(&mut self, machine: &mut Machine) -> Result<(), String> {
        if machine.powered_off() {
            return Err("The VM is already off.".into());
        }
        self.running = false;
        self.cancel_job(machine, "cancelled by power-off");
        for client in self.clients.keys().copied().collect::<Vec<_>>() {
            self.release_input(machine, client)?;
        }
        if let Err(error) = machine.power_off() {
            self.failure(machine, &error);
            return Err(error);
        }
        self.stop(machine, "{\"kind\":\"powered-off\"}".into(), false);
        self.event(machine, "power-off", "powered off by the host");
        Ok(())
    }

    fn media_data(&mut self, id: usize, data: &[u8]) -> Result<String, String> {
        let upload = self
            .upload
            .as_mut()
            .filter(|upload| upload.owner == id)
            .ok_or("No media upload in progress.")?;
        upload.write(data)?;
        Ok(format!(
            "{{\"type\":\"media-data\",\"ok\":true,\"received\":{}}}",
            upload.progress().0
        ))
    }

    // Replay cannot recreate an inserted image, so it stops at this record.
    fn media_event(
        &mut self,
        machine: &Machine,
        id: usize,
        cmd: &str,
        name: &str,
        bytes: usize,
    ) -> Result<(), String> {
        let step = machine.step_count();
        self.clients
            .get_mut(&id)
            .unwrap()
            .session
            .record_line(&format!(
                "{{\"step\":{step},\"cmd\":\"{cmd}\",\"name\":\"{}\",\"bytes\":{bytes}}}",
                json_escape(name)
            ))?;
        let text = if cmd == "media-eject" {
            "disc ejected".to_string()
        } else {
            format!("inserted {name} ({} KiB)", bytes / 1024)
        };
        self.event(machine, "media", &text);
        Ok(())
    }

    fn media_json(&self, machine: &Machine, id: usize) -> String {
        let drive = machine.cdrom_state();
        let state = drive.as_ref();
        let inserted = state.is_some_and(|s| s.inserted);
        let name = match (&self.media_name, inserted) {
            (Some(name), true) => format!("\"{}\"", json_escape(name)),
            _ => "null".into(),
        };
        let upload = self.upload.as_ref().map_or("null".into(), |upload| {
            let (received, files) = upload.progress();
            format!(
                "{{\"kind\":\"{}\",\"name\":\"{}\",\"mine\":{},\"received\":{received},\"files\":{files}}}",
                if upload.is_iso() { "iso" } else { "files" },
                json_escape(&upload.name),
                upload.owner == id
            )
        });
        format!(
            "{{\"type\":\"media\",\"ok\":true,\"step\":{},\"drive\":{},\"inserted\":{inserted},\"tray_open\":{},\"locked\":{},\"name\":{name},\"limit\":{},\"upload\":{upload}}}",
            machine.step_count(),
            drive.is_some(),
            state.is_some_and(|s| s.tray_open),
            state.is_some_and(|s| s.locked),
            MEDIA_LIMIT
        )
    }
}

// A finite debugger request is a job, not a blocking call: other clients can
// read logs, inspect the current machine, or pause it between every slice.
struct Job {
    client: usize,
    cmd: String,
    start: u64,
    deadline: u64,
    slice: u64,
    needle: String,
    matcher: ByteMatcher,
    serial_seen: usize,
    stable_needed: u64,
    stable_count: u64,
    last_hash: String,
    sampled_at: Option<u64>,
}
impl Job {
    fn is_command(cmd: &str) -> bool {
        matches!(
            cmd,
            "run-steps" | "run-until-step" | "wait-text" | "wait-serial" | "wait-frame-stable"
        )
    }
    fn new(
        client: usize,
        cmd: &str,
        message: &str,
        machine: &Machine,
        session: &WsSession,
    ) -> Result<Self, String> {
        let start = machine.step_count();
        let deadline = match cmd {
            "run-steps" => {
                start.saturating_add(json_u64(message, "steps")?.unwrap_or(session.slice_steps))
            }
            "run-until-step" => json_u64(message, "step")?
                .ok_or("run-until-step missing step")?
                .max(start),
            _ => start
                .saturating_add(json_u64(message, "timeout_steps")?.unwrap_or(session.slice_steps)),
        };
        let needle = if matches!(cmd, "wait-text" | "wait-serial") {
            json_string(message, "text")?.ok_or("wait requires text")?
        } else {
            String::new()
        };
        Ok(Self {
            client,
            cmd: cmd.into(),
            start,
            deadline,
            slice: json_u64(message, "slice_steps")?
                .unwrap_or(session.slice_steps)
                .max(1),
            matcher: ByteMatcher::new(needle.as_bytes()),
            needle,
            serial_seen: session.serial_cursor,
            stable_needed: json_u64(message, "count")?.unwrap_or(2).max(1),
            stable_count: 0,
            last_hash: String::new(),
            sampled_at: None,
        })
    }
    fn poll(
        &mut self,
        machine: &Machine,
        stop: &Option<GuestStepStatus>,
        session: &mut WsSession,
    ) -> Result<Option<String>, String> {
        // A realtime HLT waits for an actual device/input deadline. It is a
        // host scheduling yield, not completion/failure of a debugger job.
        let stop = if machine.uses_realtime_clock() && matches!(stop, Some(GuestStepStatus::Halted))
        {
            &None
        } else {
            stop
        };
        let matched = match self.cmd.as_str() {
            "wait-text" => machine.vga().text_contains(&self.needle),
            "wait-serial" => {
                let matched = self
                    .matcher
                    .observe(&machine.serial_output()[self.serial_seen..]);
                self.serial_seen = machine.serial_output().len();
                matched
            }
            "wait-frame-stable" => {
                if self.sampled_at != Some(machine.step_count()) {
                    self.sampled_at = Some(machine.step_count());
                    let hash = capture_frame(machine)?.hash;
                    if hash == self.last_hash {
                        self.stable_count += 1;
                    } else {
                        self.last_hash = hash;
                        self.stable_count = 1;
                    }
                }
                self.stable_count >= self.stable_needed
            }
            _ => false,
        };
        let timeout = machine.step_count() >= self.deadline;
        if !matched && !timeout && stop.is_none() {
            return Ok(None);
        }
        if self.cmd.starts_with("run-") {
            return Ok(Some(format!(
                "{{\"type\":\"{}\",\"ok\":{},\"step\":{},\"start_step\":{},\"end_step\":{},\"ran\":{},\"stop\":{},\"serial_hex\":\"{}\"}}",
                self.cmd,
                stop.is_none() || matches!(stop, Some(GuestStepStatus::PoweredOff)),
                machine.step_count(),
                self.start,
                machine.step_count(),
                machine.step_count() - self.start,
                stop_json(stop),
                session.take_serial_hex(machine)
            )));
        }
        let error = if matched {
            ""
        } else if stop.is_some() {
            "guest-stopped"
        } else {
            "timeout"
        };
        let mode = machine.vga().mode_info();
        Ok(Some(format!(
            "{{\"type\":\"{}\",\"ok\":{},\"step\":{},\"start_step\":{},\"error\":\"{error}\",\"stop\":{},\"text\":\"{}\",\"serial_hex\":\"{}\",\"matched_hex\":\"{}\",\"hash\":\"{}\",\"last_hash\":\"{}\",\"matched\":\"{}\",\"timeout_steps\":{},\"last_mode\":\"{}\",\"count\":{},\"width\":{},\"height\":{},\"format\":{}}}",
            self.cmd,
            matched,
            machine.step_count(),
            self.start,
            stop_json(stop),
            json_escape(&machine.text_snapshot()),
            session.take_serial_hex(machine),
            hex_encode(self.needle.as_bytes()),
            self.last_hash,
            self.last_hash,
            json_escape(&self.needle),
            self.deadline - self.start,
            mode_name(mode.mode),
            self.stable_needed,
            mode.width,
            mode.height,
            ws::PIXEL_FORMAT_RGBA
        )))
    }
}

/// A console server: socket threads feed one request channel, and the host
/// loop that owns the machine handles requests between guest slices.
/// r86-run's `serve_ws` drives execution itself; the handheld calls `poll`
/// and `run_slice` from its own display loop.
pub struct Remote {
    rx: mpsc::Receiver<Request>,
    service: Service,
    slice_steps: u64,
    record: Option<Arc<fs::File>>,
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Remote {
    /// Serves the console on `listener`, open to the local network like a
    /// home BMC. `jobs` enables finite debugger jobs, which need a host loop
    /// that steps the machine for them. With `save_dir`, uploaded discs are
    /// saved there instead of held in memory.
    pub fn new(
        listener: TcpListener,
        slice_steps: u64,
        record_path: Option<&str>,
        jobs: bool,
        save_dir: Option<PathBuf>,
    ) -> Result<Self, String> {
        let record = record_path
            .map(|path| {
                fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(path)
                    .map(Arc::new)
                    .map_err(|e| format!("failed to open websocket record {path}: {e}"))
            })
            .transpose()?;
        let (rx, port, stop) = listen(listener)?;
        let mut service = Service::new();
        service.jobs = jobs;
        service.save_dir = save_dir;
        Ok(Self {
            rx,
            service,
            slice_steps,
            record,
            port,
            stop,
        })
    }

    /// The address other devices on the network can open.
    #[allow(dead_code)] // The handheld's embedding API; r86-run drives itself.
    pub fn url(&self) -> String {
        let host = lan_address().unwrap_or_else(|| "this-device".into());
        format!("http://{host}:{}/", self.port)
    }

    /// Whether the console wants the guest to run (Resume, Power on) or not
    /// (Pause, Power off, a stop).
    #[allow(dead_code)]
    pub fn running(&self) -> bool {
        self.service.running
    }

    /// Boots a powered-off VM, as the console's Power on does.
    #[allow(dead_code)]
    pub fn power_on(&mut self, machine: &mut Machine) -> Result<(), String> {
        self.service.power_on(machine)
    }

    /// Cuts power, as the console's Power off does.
    #[allow(dead_code)]
    pub fn power_off(&mut self, machine: &mut Machine) -> Result<(), String> {
        self.service.power_off(machine)
    }

    /// Reboots from the device's own menu, as the console's reset does, but
    /// keeps the VM running.
    #[allow(dead_code)]
    pub fn reset(&mut self, machine: &mut Machine) -> Result<(), String> {
        self.service.cancel_text(machine, None, "reset");
        machine.reboot()?;
        self.service.frame = None;
        self.service
            .event(machine, "reset", "reset from the device");
        Ok(())
    }

    /// Records a disc change made on the device.
    #[allow(dead_code)]
    pub fn media_changed(&mut self, machine: &Machine, name: Option<String>) {
        let text = name
            .as_deref()
            .map_or("disc ejected".into(), |n| format!("inserted {n}"));
        self.service.media_name = name;
        self.service.event(machine, "media", &text);
    }

    /// A disc a browser inserted or ejected since the last call: the saved
    /// image, or None for an ejected or in-memory disc.
    #[allow(dead_code)]
    pub fn take_disc_change(&mut self) -> Option<Option<PathBuf>> {
        self.service.disc_change.take()
    }

    /// Stops serving, first releasing keys and buttons browsers still hold.
    #[allow(dead_code)]
    pub fn close(mut self, machine: &mut Machine) {
        for id in self.service.clients.keys().copied().collect::<Vec<_>>() {
            let _ = self.service.release_input(machine, id);
        }
    }

    /// Connected browsers and other WebSocket clients.
    #[allow(dead_code)]
    pub fn clients(&self) -> usize {
        self.service.clients.len()
    }

    #[allow(dead_code)]
    pub fn set_running(&mut self, machine: &Machine, running: bool) {
        self.service.running = running;
        let text = if running { "running" } else { "paused" };
        self.service.event(machine, "execution", text);
    }

    /// Handles every queued request without blocking.
    pub fn poll(&mut self, machine: &mut Machine) {
        while let Ok(request) = self.rx.try_recv() {
            self.handle(machine, request);
        }
    }

    /// Runs one guest slice for a host loop that drives execution itself,
    /// delivering typed text and recording stops for the console.
    #[allow(dead_code)]
    pub fn run_slice(
        &mut self,
        machine: &mut Machine,
        budget: u64,
    ) -> Result<r86::machine::RunReport, String> {
        if let Err(error) = self.service.deliver_text(machine, self.record.as_deref()) {
            self.service.failure(machine, &error);
            return Err(error);
        }
        let reset_count = machine.reset_count();
        let result = machine.run_steps(budget);
        self.service.after_run(machine, &result, reset_count);
        result
    }

    fn handle(&mut self, machine: &mut Machine, request: Request) {
        let service = &mut self.service;
        match request {
            Request::Connect(id, reply) => {
                let session = WsSession::new(self.slice_steps, self.record.clone());
                let mut hello = Reply::text(session.hello(machine));
                hello.text.pop();
                hello.text.push_str(&format!(
                    ",\"client_id\":{id},\"execution\":{}}}",
                    service.state(machine)
                ));
                service.clients.insert(id, Client { session, reply });
                service.reply(id, hello);
            }
            Request::Disconnect(id) => {
                if let Err(error) = service.release_input(machine, id) {
                    service.failure(machine, &error);
                }
                if service.job.as_ref().is_some_and(|job| job.client == id) {
                    service.cancel_job(machine, "client disconnected");
                }
                if service
                    .upload
                    .as_ref()
                    .is_some_and(|upload| upload.owner == id)
                {
                    service.upload = None;
                }
                // An unfinished copy leaves no partial file behind.
                if let Some(put) = service.file_put.take_if(|put| put.owner == id)
                    && machine.powered_off()
                    && let Some(disk) = machine.hdd_mut()
                {
                    let _ = disk_files::delete(disk, &put.path);
                }
                service.clients.remove(&id);
            }
            Request::Data(id, data) => {
                if !service.clients.contains_key(&id) {
                    return;
                }
                let text = if service.file_put.as_ref().is_some_and(|put| put.owner == id) {
                    service.file_data(machine, &data)
                } else {
                    service.media_data(id, &data)
                }
                .unwrap_or_else(|error| error_json(machine.step_count(), &error));
                service.reply(id, Reply::text(text));
            }
            Request::Command(id, message) => {
                if !service.clients.contains_key(&id) {
                    return;
                }
                match service.command(machine, id, &message) {
                    Ok(Some(reply)) => service.reply(id, reply),
                    Ok(None) => {}
                    Err(error) => {
                        service.event(machine, "command-error", &error);
                        service.reply(id, Reply::text(error_json(machine.step_count(), &error)))
                    }
                }
            }
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // Dropping the service closes every client's reply channel.
        stop_listening(&self.stop, self.port);
    }
}

/// The console page without a machine: receives one disk or CD image from a
/// browser for the handheld's New VM wizard and saves it in `dir`, named as
/// in media.rs. Same page, port range and transport as `Remote`.
#[allow(dead_code)] // The handheld's; r86-run has no wizard.
pub struct Import {
    rx: mpsc::Receiver<Request>,
    port: u16,
    stop: Arc<AtomicBool>,
    kind: ImageKind,
    dir: PathBuf,
    clients: BTreeMap<usize, mpsc::SyncSender<Reply>>,
    /// The image arriving and its announced size.
    upload: Option<(Upload, u64)>,
    saved: Option<PathBuf>,
}

impl Import {
    pub fn new(listener: TcpListener, kind: ImageKind, dir: PathBuf) -> Result<Self, String> {
        let (rx, port, stop) = listen(listener)?;
        Ok(Self {
            rx,
            port,
            stop,
            kind,
            dir,
            clients: BTreeMap::new(),
            upload: None,
            saved: None,
        })
    }

    /// The page's address on the local network, if this device has one.
    pub fn url(&self) -> Option<String> {
        lan_address().map(|host| format!("http://{host}:{}/", self.port))
    }

    /// The image arriving: its name, bytes received and size.
    pub fn progress(&self) -> Option<(&str, u64, u64)> {
        let (upload, size) = self.upload.as_ref()?;
        Some((&upload.name, upload.progress().0, *size))
    }

    /// Handles queued requests; returns the saved image once a browser has
    /// sent all of it.
    pub fn poll(&mut self) -> Option<PathBuf> {
        while let Ok(request) = self.rx.try_recv() {
            self.handle(request);
        }
        self.saved.take()
    }

    fn handle(&mut self, request: Request) {
        let (id, text) = match request {
            Request::Connect(id, reply) => {
                self.clients.insert(id, reply);
                let hello = format!(
                    "{{\"type\":\"hello\",\"ok\":true,\"version\":1,\"client_id\":{id},\"import\":\"{}\"}}",
                    self.kind.name()
                );
                (id, Ok(hello))
            }
            Request::Disconnect(id) => {
                self.clients.remove(&id);
                if self
                    .upload
                    .as_ref()
                    .is_some_and(|(upload, _)| upload.owner == id)
                {
                    self.upload = None;
                }
                return;
            }
            Request::Command(id, message) => (id, self.command(id, &message)),
            Request::Data(id, data) => (id, self.data(id, &data)),
        };
        let text = text.unwrap_or_else(|error| error_json(0, &error));
        if let Some(client) = self.clients.get(&id) {
            let _ = client.try_send(Reply::text(text));
        }
    }

    fn command(&mut self, id: usize, message: &str) -> Result<String, String> {
        let mine = self
            .upload
            .as_ref()
            .is_some_and(|(upload, _)| upload.owner == id);
        match json_string(message, "cmd")?.as_deref() {
            Some("import-begin") => {
                if self.upload.is_some() && !mine {
                    return Err("Another browser is sending an image.".into());
                }
                let name = json_string(message, "name")?.unwrap_or_default();
                let size = json_u64(message, "size")?.ok_or("import-begin missing size")?;
                let upload = match self.kind {
                    ImageKind::Cd => Upload::iso(id, &name, size, Some(&self.dir))?,
                    ImageKind::Disk => Upload::disk(id, &name, size, &self.dir)?,
                };
                self.upload = Some((upload, size));
                Ok("{\"type\":\"import\",\"ok\":true}".to_string())
            }
            Some("import-end") if mine => {
                let (upload, _) = self.upload.take().unwrap();
                let Image::File(_, path) = upload.finish(Some(&self.dir))? else {
                    unreachable!("imports are saved")
                };
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let reply = format!(
                    "{{\"type\":\"import\",\"ok\":true,\"name\":\"{}\"}}",
                    json_escape(&name)
                );
                self.saved = Some(path);
                Ok(reply)
            }
            Some("import-end") => Err("No image is being sent.".into()),
            Some("import-cancel") => {
                if mine {
                    self.upload = None;
                }
                Ok("{\"type\":\"import\",\"ok\":true}".to_string())
            }
            _ => Err("This page only receives an image: no VM is running.".into()),
        }
    }

    fn data(&mut self, id: usize, data: &[u8]) -> Result<String, String> {
        let (upload, _) = self
            .upload
            .as_mut()
            .filter(|(upload, _)| upload.owner == id)
            .ok_or("No image is being sent.")?;
        upload.write(data)?;
        Ok(format!(
            "{{\"type\":\"import-data\",\"ok\":true,\"received\":{}}}",
            upload.progress().0
        ))
    }
}

impl Drop for Import {
    fn drop(&mut self) {
        stop_listening(&self.stop, self.port);
    }
}

/// Accepts page and WebSocket connections on `listener`; each socket gets its
/// own worker thread, which owns only the socket and bounded messages.
fn listen(
    listener: TcpListener,
) -> Result<(mpsc::Receiver<Request>, u16, Arc<AtomicBool>), String> {
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let (tx, rx) = mpsc::sync_channel(64);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    std::thread::spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        for (id, stream) in listener.incoming().enumerate() {
            if stopping.load(Ordering::Relaxed) {
                break;
            }
            let Ok(stream) = stream else {
                break;
            };
            if active.load(Ordering::Relaxed) >= 32 {
                continue;
            }
            active.fetch_add(1, Ordering::Relaxed);
            let active = active.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = connection(stream, id, &tx);
                let _ = tx.send(Request::Disconnect(id));
                active.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });
    Ok((rx, port, stop))
}

/// Wakes the accept loop so it sees the stop flag.
fn stop_listening(stop: &AtomicBool, port: u16) {
    stop.store(true, Ordering::Relaxed);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
}

/// The address other devices reach this host at: the source address of the
/// default route (connecting a UDP socket sends nothing), else, on a LAN
/// without one, an interface's IPv4 address, preferring interfaces that are up.
#[allow(dead_code)]
fn lan_address() -> Option<String> {
    let routed = || {
        let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
        socket.connect(("192.0.2.1", 9)).ok()?;
        let ip = socket.local_addr().ok()?.ip();
        (!ip.is_unspecified()).then(|| ip.to_string())
    };
    routed().or_else(interface_address)
}

#[allow(dead_code)]
fn interface_address() -> Option<String> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    let mut best: Option<(bool, std::net::Ipv4Addr)> = None;
    // SAFETY: getifaddrs returns a list this function only reads and then
    // releases with freeifaddrs; AF_INET entries hold a sockaddr_in.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut entry = list;
        while let Some(ifa) = entry.as_ref() {
            entry = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET {
                continue;
            }
            let addr = &*ifa.ifa_addr.cast::<libc::sockaddr_in>();
            let ip = std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
            let up = ifa.ifa_flags & libc::IFF_UP as libc::c_uint != 0;
            if !ip.is_loopback() && best.is_none_or(|(best_up, _)| up && !best_up) {
                best = Some((up, ip));
            }
        }
        libc::freeifaddrs(list);
    }
    best.map(|(_, ip)| ip.to_string())
}

pub fn serve_ws(
    addr: &str,
    slice_steps: u64,
    record_path: Option<&str>,
    machine: &mut Machine,
) -> Result<(), String> {
    let listener = TcpListener::bind(addr).map_err(|e| format!("web server {addr}: {e}"))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    println!("web console: http://{local}/ (WebSocket: ws://{local}/ws)");
    let mut remote = Remote::new(listener, slice_steps, record_path, true, None)?;
    remote
        .service
        .event(machine, "start", "server ready; paused");
    loop {
        if remote.service.running || remote.service.job.is_some() {
            remote.poll(machine);
        } else {
            let request = remote.rx.recv().map_err(|e| e.to_string())?;
            remote.handle(machine, request);
        }
        let service = &mut remote.service;
        let mut job = service.job.take();
        if let Some(ref mut job) = job {
            match job.poll(
                machine,
                &None,
                &mut service.clients.get_mut(&job.client).unwrap().session,
            ) {
                Ok(Some(text)) => {
                    service.reply(job.client, Reply::text(text));
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    service.reply(
                        job.client,
                        Reply::text(error_json(machine.step_count(), &error)),
                    );
                    continue;
                }
            }
        }
        if !service.running && job.is_none() {
            continue;
        }
        if let Err(error) = service.deliver_text(machine, remote.record.as_deref()) {
            service.failure(machine, &error);
            if let Some(job) = job {
                service.reply(
                    job.client,
                    Reply::text(error_json(machine.step_count(), &error)),
                );
            }
            continue;
        }
        let budget = job
            .as_ref()
            .map_or(slice_steps, |job| {
                (job.deadline - machine.step_count()).min(job.slice)
            })
            .min(slice_steps);
        let reset_count = machine.reset_count();
        let result = machine.run_steps(budget);
        service.after_run(machine, &result, reset_count);
        match result {
            Ok(report) => {
                if let Some(mut job) = job {
                    match job.poll(
                        machine,
                        &report.stop,
                        &mut service.clients.get_mut(&job.client).unwrap().session,
                    ) {
                        Ok(Some(text)) => service.reply(job.client, Reply::text(text)),
                        Ok(None) => service.job = Some(job),
                        Err(error) => service.reply(
                            job.client,
                            Reply::text(error_json(machine.step_count(), &error)),
                        ),
                    }
                }
                if matches!(report.stop, Some(GuestStepStatus::PoweredOff)) {
                    // The machine flushed its disks at S5. The server stays up,
                    // like a BMC: files, media and power-on remain available.
                    println!("guest powered off after {} steps", machine.step_count());
                }
                if matches!(report.stop, Some(GuestStepStatus::Halted)) {
                    std::thread::sleep(machine.halted_wait_duration());
                }
            }
            Err(error) => {
                if let Some(job) = job {
                    service.reply(
                        job.client,
                        Reply::text(error_json(machine.step_count(), &error)),
                    );
                }
            }
        }
    }
}

fn connection(
    stream: std::net::TcpStream,
    id: usize,
    tx: &mpsc::SyncSender<Request>,
) -> Result<(), String> {
    let Some(mut socket) = ws::WebSocket::accept(stream)? else {
        return Ok(());
    };
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    tx.send(Request::Connect(id, reply_tx))
        .map_err(|e| e.to_string())?;
    let mut reader = socket.reader()?;
    let requests = tx.clone();
    let in_flight = Arc::new(AtomicBool::new(false));
    let reading = in_flight.clone();
    // A separate reader notices disconnect even during a long debugger job.
    // Request pipelining is rejected; every command owns its JSON/binary reply.
    let reader_thread = std::thread::spawn(move || {
        loop {
            let request = match reader.read_message() {
                Ok(ws::Message::Text(text)) => Request::Command(id, text),
                Ok(ws::Message::Binary(data)) => Request::Data(id, data),
                _ => break,
            };
            if reading.swap(true, Ordering::SeqCst) || requests.send(request).is_err() {
                break;
            }
        }
        let _ = requests.send(Request::Disconnect(id));
    });
    let result = (|| {
        while let Ok(reply) = reply_rx.recv() {
            in_flight.store(false, Ordering::SeqCst);
            socket.send_text(&reply.text)?;
            if let Some(binary) = reply.binary {
                socket.send_binary(&binary)?;
            }
            if reply.close {
                break;
            }
        }
        Ok(())
    })();
    socket.shutdown();
    let _ = reader_thread.join();
    result
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    /// A browser reduced to what the import page sends.
    struct Browser(TcpStream);

    impl Browser {
        fn connect(port: u16) -> Self {
            let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
            socket
                .write_all(
                    b"GET /ws HTTP/1.1\r\nHost: r86\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                      Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
                )
                .unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            assert!(head.starts_with(b"HTTP/1.1 101"));
            Self(socket)
        }

        fn send(&mut self, opcode: u8, payload: &[u8]) {
            let mut frame = vec![0x80 | opcode];
            match payload.len() {
                len @ 0..126 => frame.push(0x80 | len as u8),
                len => {
                    frame.push(0x80 | 126);
                    frame.extend((len as u16).to_be_bytes());
                }
            }
            // A zero mask leaves the payload unchanged.
            frame.extend([0; 4]);
            frame.extend(payload);
            self.0.write_all(&frame).unwrap();
        }

        fn call(&mut self, opcode: u8, payload: &[u8]) -> String {
            self.send(opcode, payload);
            self.recv()
        }

        fn recv(&mut self) -> String {
            let mut head = [0; 2];
            self.0.read_exact(&mut head).unwrap();
            let len = match head[1] & 0x7f {
                126 => {
                    let mut len = [0; 2];
                    self.0.read_exact(&mut len).unwrap();
                    usize::from(u16::from_be_bytes(len))
                }
                len => usize::from(len),
            };
            let mut payload = vec![0; len];
            self.0.read_exact(&mut payload).unwrap();
            String::from_utf8(payload).unwrap()
        }
    }

    /// Runs `browse` against `import`, polling it as the wizard does.
    fn serve(import: &mut Import, browse: impl FnOnce(u16) + Send + 'static) -> Option<PathBuf> {
        let browser = std::thread::spawn({
            let port = import.port;
            move || browse(port)
        });
        let mut saved = None;
        while !browser.is_finished() {
            saved = saved.or(import.poll());
            std::thread::sleep(Duration::from_millis(1));
        }
        browser.join().unwrap();
        // Let the disconnect arrive.
        std::thread::sleep(Duration::from_millis(20));
        saved.or(import.poll())
    }

    #[test]
    fn import_saves_one_image_and_drops_an_abandoned_one() {
        let dir = std::env::temp_dir().join(format!("r86-import-{}", std::process::id()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut import = Import::new(listener, ImageKind::Disk, dir.clone()).unwrap();
        let saved = serve(&mut import, |port| {
            let mut browser = Browser::connect(port);
            assert!(browser.recv().contains("\"import\":\"disk\""));
            let begin = r#"{"cmd":"import-begin","name":"My Disk.IMG","size":1024}"#;
            assert!(browser.call(1, begin.as_bytes()).contains("\"ok\":true"));
            assert!(browser.call(2, &[0x55; 512]).contains("\"received\":512"));
            assert!(browser.call(2, &[0x55; 512]).contains("\"received\":1024"));
            let end = browser.call(1, br#"{"cmd":"import-end"}"#);
            assert!(end.contains("\"name\":\"My Disk-"), "{end}");
        });
        let saved = saved.expect("the wizard gets the image");
        assert_eq!(fs::read(&saved).unwrap(), [0x55; 1024]);
        let name = saved.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("My Disk-") && name.ends_with(".img"),
            "{name}"
        );

        // A browser that leaves mid-image leaves nothing behind.
        let abandoned = serve(&mut import, |port| {
            let mut browser = Browser::connect(port);
            browser.recv();
            let begin = r#"{"cmd":"import-begin","name":"half.vhd","size":1024}"#;
            assert!(browser.call(1, begin.as_bytes()).contains("\"ok\":true"));
            browser.call(2, &[1; 512]);
        });
        assert!(abandoned.is_none());
        let files: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(files, [saved]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
