//! JSON and frame helpers shared by the WebSocket console (r86-run and the
//! handheld) and r86-run's command line: field parsing for the small,
//! fixed command schema in script/ws_cli.py, status/stop/frame objects, and
//! a streaming byte matcher for serial waits.

use r86::devices::{Ps2ButtonState, RenderMode};
use r86::machine::{GuestStepStatus, Machine};
use r86::x86::Reg32;
use r86::x86::cpu::SegmentReg;

use super::ws;

/// A VM-wide interval. Guest resets retain counters; the owner cancels before
/// replacing the Machine or its engine. No host clock enters the core.
#[cfg(debug_assertions)]
pub struct Profile {
    wall: std::time::Instant,
    step: u64,
    idle: u64,
    engine: &'static str,
    #[cfg(a64_jit)]
    jit: Option<r86::x86::jit::runtime::Stats>,
    #[cfg(a64_jit)]
    clears: Option<r86::machine::JitCacheCounters>,
}

#[cfg(debug_assertions)]
impl Profile {
    pub fn start(machine: &mut Machine) -> Self {
        #[cfg(a64_jit)]
        machine.set_jit_profiling(true);
        Self {
            step: machine.step_count(),
            idle: machine.profile_idle_steps(),
            engine: machine.engine_label(),
            #[cfg(a64_jit)]
            jit: machine.profile_jit_stats().cloned(),
            #[cfg(a64_jit)]
            clears: machine.jit_cache_counters(),
            wall: std::time::Instant::now(),
        }
    }

    pub fn cancel(self, _machine: &mut Machine) {
        #[cfg(a64_jit)]
        _machine.set_jit_profiling(false);
    }

    pub fn finish(self, machine: &mut Machine) -> String {
        use std::fmt::Write;
        let seconds = self.wall.elapsed().as_secs_f64();
        #[cfg(a64_jit)]
        let exits = machine.set_jit_profiling(false);
        let steps = machine.step_count() - self.step;
        let idle = machine.profile_idle_steps() - self.idle;
        let work = steps - idle;
        let mut out = format!(
            "# Session profile\n\nEngine: {}. Steps {} to {}.\n\n## 1. Work and time\n\nScheduled steps: {steps}\n\nIdle steps: {idle}\n\nExecuted/attempted steps: {work}\n\nWall seconds: {seconds:.6}\n\n",
            self.engine,
            self.step,
            machine.step_count(),
        );
        if seconds > 0.0 {
            writeln!(out, "Guest MIPS: {:.3}\n", work as f64 / seconds / 1e6).unwrap();
        } else {
            out.push_str("Guest MIPS: unavailable (zero elapsed time)\n\n");
        }
        out.push_str("Wall time includes pauses and frontend work. Work counts attempted instructions; halted polls and deterministic idle advances are excluded.\n\n");
        #[cfg(a64_jit)]
        if let Some(before) = self.jit {
            use r86::x86::jit::runtime::{ExitReason, ExitSites};
            let after = machine
                .profile_jit_stats()
                .expect("profile owner replaced engine");
            let native = after.native_steps - before.native_steps;
            let interpreted = work - native;
            let causes: [u64; 4] = core::array::from_fn(|i| {
                after.interpreted_causes[i] - before.interpreted_causes[i]
            });
            writeln!(out, "## 2. Native and interpreted work\n\nNative steps: {native}\n\nInterpreted steps: {interpreted}\n\nRaw JIT-adapter interpreted steps (includes halted polls): {}\n\n| Interpreted cause | Steps |\n| --- | ---: |", after.interpreted_steps - before.interpreted_steps).unwrap();
            for (name, count) in [
                "ineligible entry",
                "cold",
                "after native exit",
                "other fallback",
            ]
            .iter()
            .zip(causes)
            {
                writeln!(out, "| {name} | {count} |").unwrap();
            }
            writeln!(
                out,
                "| outside JIT adapter | {} |\n",
                interpreted - causes.iter().sum::<u64>()
            )
            .unwrap();
            out.push_str("Causes describe batch entry, not each instruction's mode. Other fallback includes unsupported instructions, unavailable fetch proofs and shadows.\n\n");
            let exits = exits.expect("profile owner disabled exit collection");
            let total: u64 = exits.reasons.iter().sum();
            let mapped: u64 = exits.sites.values().sum();
            writeln!(out, "## 3. Native returns\n\nNative returns: {}\n\n| Reason | Returns |\n| --- | ---: |", after.native_invocations - before.native_invocations).unwrap();
            for (name, count) in ExitReason::NAMES.iter().zip(exits.reasons) {
                writeln!(out, "| {name} | {count} |").unwrap();
            }
            out.push_str("\nReasons cover every native return, including continuations inside the JIT. Memory/guard does not imply a guest page fault; attention includes shadow boundaries.\n\n");
            writeln!(out, "## 4. Compilation and retained code\n\nBlocks compiled: {}\n\nCode bytes compiled: {}\n\nCode-change invalidations (compiled page records): {}\n\nUnchanged-content validations retaining entries: {}\n\nThese are cumulative publication bytes and invalidated records, not live occupancy or exact blocks recompiled. Validations do not always install a new watch.\n", after.compiled_blocks - before.compiled_blocks, after.code_bytes - before.code_bytes, after.content_changes - before.content_changes, after.content_reuses - before.content_reuses).unwrap();
            let clears = machine.jit_cache_counters().unwrap();
            let initial = self.clears.unwrap();
            out.push_str("## 5. Cache clears\n\n| Cause | Clears |\n| --- | ---: |\n");
            for (i, name) in [
                "code area full",
                "page-record limit",
                "no free entry table",
                "external reset",
            ]
            .iter()
            .enumerate()
            {
                writeln!(out, "| {name} | {} |", clears.causes[i] - initial.causes[i]).unwrap();
            }
            writeln!(out, "\n## 6. Top 20 retained exit sites\n\nRetained keys: {} / 4096. Total returns: {total}. Mapped returns: {mapped}. Overflow returns: {}.\n\nSites randomly sample 1 in {} native returns on average; counts are unscaled samples. Reason totals above are exact. Overflow is total minus mapped: it includes unsampled returns and samples omitted by the cap.\n\nCS:EIP is the committed return/resume site. Equal selectors and offsets across address spaces are merged. After the cap, existing keys keep counting samples; unseen keys are omitted. This is not necessarily the global top 20.\n\n| CS:EIP | Reason | Sampled returns |\n| --- | --- | ---: |", exits.sites.len(), total - mapped, ExitSites::SAMPLE_INTERVAL).unwrap();
            let mut sites: Vec<_> = exits.sites.into_iter().collect();
            sites.sort_unstable_by(|(ka, ca), (kb, cb)| cb.cmp(ca).then(ka.cmp(kb)));
            for ((cs, eip, reason), count) in sites.into_iter().take(20) {
                writeln!(
                    out,
                    "| {cs:04x}:{eip:08x} | {} | {count} |",
                    ExitReason::NAMES[reason as usize]
                )
                .unwrap();
            }
            return out;
        }
        out.push_str("## 2. Native and interpreted work\n\n");
        if !machine.jit_enabled() {
            writeln!(out, "Interpreted steps (interpreter selected): {work}\n").unwrap();
        }
        out.push_str("A64 native and fallback counters: not applicable.\n\n## 3. Native returns\n\nNot applicable: A64 JIT is not selected.\n\n## 4. Compilation and retained code\n\nNot applicable: A64 JIT is not selected.\n\n## 5. Cache clears\n\nNot applicable: A64 JIT is not selected.\n\n## 6. Top 20 retained exit sites\n\nNot applicable: A64 JIT is not selected.\n");
        out
    }
}

pub struct ByteMatcher {
    needle: Vec<u8>,
    fallback: Vec<usize>,
    matched: usize,
    done: bool,
}

impl ByteMatcher {
    pub fn new(needle: &[u8]) -> Self {
        let mut fallback = vec![0; needle.len()];
        let mut matched = 0usize;
        for index in 1..needle.len() {
            while matched > 0 && needle[index] != needle[matched] {
                matched = fallback[matched - 1];
            }
            if needle[index] == needle[matched] {
                matched += 1;
            }
            fallback[index] = matched;
        }
        Self {
            needle: needle.to_vec(),
            fallback,
            matched: 0,
            done: needle.is_empty(),
        }
    }

    pub fn observe(&mut self, bytes: &[u8]) -> bool {
        if self.done {
            return true;
        }
        for &byte in bytes {
            while self.matched > 0 && byte != self.needle[self.matched] {
                self.matched = self.fallback[self.matched - 1];
            }
            if byte == self.needle[self.matched] {
                self.matched += 1;
                if self.matched == self.needle.len() {
                    self.done = true;
                    return true;
                }
            }
        }
        false
    }
}

pub struct FrameCapture {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixels: Vec<u8>,
    pub hash: String,
}

pub fn capture_frame(machine: &Machine) -> Result<FrameCapture, String> {
    let info = machine.vga().mode_info();
    let width = u32::from(info.width);
    let height = u32::from(info.height);
    let len = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| "VGA frame dimensions overflow host usize".to_string())?;
    let mut pixels = vec![0u8; len];
    let rendered = machine
        .vga()
        .render_rgba(&mut pixels)
        .ok_or_else(|| "VGA renderer rejected the frame buffer".to_string())?;
    let hash = ws::frame_hash_hex(
        u32::from(rendered.width),
        u32::from(rendered.height),
        ws::PIXEL_FORMAT_RGBA,
        &pixels,
    );
    Ok(FrameCapture {
        width: u32::from(rendered.width),
        height: u32::from(rendered.height),
        stride: u32::from(rendered.width) * 4,
        pixels,
        hash,
    })
}

pub fn status_json(machine: &Machine, ok: bool) -> String {
    let devices = machine.device_status();
    let mode = machine.vga().mode_info();
    let cs = machine.cpu().seg(SegmentReg::Cs);
    format!(
        "{{\"type\":\"status\",\"ok\":{},\"step\":{},\"clock\":\"{}\",\"engine\":\"{}\",\"cpu\":{{\"eip\":{},\"eax\":{},\"ebx\":{},\"ecx\":{},\"edx\":{},\"esi\":{},\"edi\":{},\"ebp\":{},\"esp\":{},\"eflags\":{},\"cr0\":{},\"cr2\":{},\"cr3\":{},\"cr4\":{},\"cs\":{},\"cs_base\":{},\"cs_limit\":{},\"cs_flags\":{},\"idtr_base\":{},\"idtr_limit\":{},\"gdtr_base\":{},\"gdtr_limit\":{},\"halted\":{}}},\"serial_len\":{},\"vga\":{{\"mode\":\"{}\",\"width\":{},\"height\":{},\"bpp\":{},\"stride\":{},\"dirty\":{}}},\"pic\":{{\"mask1\":{},\"mask2\":{},\"isr1\":{},\"isr2\":{},\"base1\":{},\"base2\":{}}},\"rtc\":{{\"irq8\":{},\"a\":{},\"b\":{},\"c\":{},\"next_tick\":{}}},\"ps2\":{{\"irq1\":{},\"irq12\":{},\"kbd_dropped\":{},\"aux_dropped\":{}}},\"reset_requested\":{},\"reset_count\":{},\"powered_off\":{}}}",
        ok,
        machine.step_count(),
        if machine.uses_realtime_clock() {
            "realtime"
        } else {
            "deterministic"
        },
        machine.engine_label(),
        machine.cpu().eip,
        machine.cpu().reg(Reg32::Eax),
        machine.cpu().reg(Reg32::Ebx),
        machine.cpu().reg(Reg32::Ecx),
        machine.cpu().reg(Reg32::Edx),
        machine.cpu().reg(Reg32::Esi),
        machine.cpu().reg(Reg32::Edi),
        machine.cpu().reg(Reg32::Ebp),
        machine.cpu().reg(Reg32::Esp),
        machine.cpu().effective_eflags(),
        machine.cpu().cr0,
        machine.cpu().cr2,
        machine.cpu().cr3,
        machine.cpu().cr4,
        cs.selector,
        cs.base,
        cs.limit,
        cs.architectural_flags(),
        machine.cpu().idtr.base,
        machine.cpu().idtr.limit,
        machine.cpu().gdtr.base,
        machine.cpu().gdtr.limit,
        machine.cpu().halted,
        machine.serial_end(),
        mode_name(mode.mode),
        mode.width,
        mode.height,
        mode.bpp,
        mode.stride,
        machine.vga().whole_frame_dirty(),
        devices.pic1_mask,
        devices.pic2_mask,
        devices.pic1_in_service,
        devices.pic2_in_service,
        devices.pic1_vector_base,
        devices.pic2_vector_base,
        devices.rtc_irq8,
        devices.cmos_status_a,
        devices.cmos_status_b,
        devices.cmos_status_c,
        devices.rtc_next_tick,
        devices.ps2_irq1,
        devices.ps2_irq12,
        devices.keyboard_dropped,
        devices.mouse_dropped,
        devices.reset_requested,
        machine.reset_count(),
        machine.powered_off()
    )
}

pub fn guest_bytes_json(machine: &mut Machine, ok: bool) -> String {
    let start = machine.cpu().eip.saturating_sub(24);
    let linear = machine.cpu().seg(SegmentReg::Cs).base.wrapping_add(start);
    let mut code = [None; 64];
    machine.guest_code_bytes(linear, &mut code, true);
    let bytes: Vec<u8> = code
        .into_iter()
        .take_while(Option::is_some)
        .flatten()
        .collect();
    format!(
        "{{\"type\":\"guest-bytes\",\"ok\":{},\"step\":{},\"eip\":{},\"start\":{},\"hex\":\"{}\"}}",
        ok,
        machine.step_count(),
        machine.cpu().eip,
        start,
        hex_encode(&bytes)
    )
}

pub fn text_snapshot_json(machine: &Machine, ok: bool) -> String {
    let snap = machine.vga().text_snapshot();
    let text = machine.text_snapshot();
    format!(
        "{{\"type\":\"text-snapshot\",\"ok\":{},\"step\":{},\"cols\":{},\"rows\":{},\"cursor_offset\":{},\"mode\":\"{}\",\"text\":\"{}\"}}",
        ok,
        machine.step_count(),
        snap.cols,
        snap.rows,
        snap.cursor_offset,
        mode_name(machine.vga().mode_info().mode),
        json_escape(&text)
    )
}

pub fn frame_hash_json(step: u64, frame: &FrameCapture, ok: bool) -> String {
    format!(
        "{{\"type\":\"frame\",\"ok\":{},\"step\":{},\"width\":{},\"height\":{},\"stride\":{},\"format\":{},\"hash\":\"{}\",\"binary\":\"rgba-frame-v1\"}}",
        ok,
        step,
        frame.width,
        frame.height,
        frame.stride,
        ws::PIXEL_FORMAT_RGBA,
        frame.hash
    )
}

pub fn stop_json(stop: &Option<GuestStepStatus>) -> String {
    match stop {
        None => "null".to_string(),
        Some(GuestStepStatus::Continue) => "{\"kind\":\"continue\"}".to_string(),
        Some(GuestStepStatus::Halted) => "{\"kind\":\"halted\"}".to_string(),
        Some(GuestStepStatus::Reset) => "{\"kind\":\"reset\"}".to_string(),
        Some(GuestStepStatus::PoweredOff) => "{\"kind\":\"powered-off\"}".to_string(),
        Some(GuestStepStatus::Stopped { message, eip }) => format!(
            "{{\"kind\":\"stopped\",\"message\":\"{}\",\"eip\":{}}}",
            json_escape(message),
            eip.map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string())
        ),
    }
}

pub fn error_json(step: u64, error: &str) -> String {
    format!(
        "{{\"type\":\"error\",\"ok\":false,\"step\":{},\"error\":\"{}\"}}",
        step,
        json_escape(error)
    )
}

pub fn mode_name(mode: RenderMode) -> &'static str {
    match mode {
        RenderMode::Text => "text",
        RenderMode::Chain4 => "chain4",
        RenderMode::Planar => "planar",
        RenderMode::Vbe => "vbe",
        RenderMode::Blank => "blank",
    }
}

pub fn set_mouse_button(
    buttons: &mut Ps2ButtonState,
    button: &str,
    down: bool,
) -> Result<(), String> {
    match button.trim().to_ascii_lowercase().as_str() {
        "left" => buttons.left = down,
        "right" => buttons.right = down,
        "middle" => buttons.middle = down,
        _ => return Err(format!("unsupported mouse button {button}")),
    }
    Ok(())
}

pub fn clamp_i64_to_i16(value: i64) -> i16 {
    value.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}

pub fn parse_hex_bytes(hex: &str) -> Result<Vec<u8>, String> {
    let digits: String = hex.chars().filter(|ch| !ch.is_ascii_whitespace()).collect();
    if digits.len() % 2 != 0 {
        return Err("hex input must have an even number of digits".to_string());
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.as_bytes().chunks(2) {
        let hi = hex_nibble(pair[0]).ok_or_else(|| "invalid hex digit".to_string())?;
        let lo = hex_nibble(pair[1]).ok_or_else(|| "invalid hex digit".to_string())?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

/// Memory ranges must not silently truncate fractions or exponents.
pub fn memory_integer(json: &str, key: &str) -> Result<Option<u64>, String> {
    let Some(raw) = json_value(json, key)? else {
        return Ok(None);
    };
    let raw = raw.split([',', '}']).next().unwrap().trim();
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("JSON key {key} is not an unsigned integer"));
    }
    raw.parse()
        .map(Some)
        .map_err(|_| format!("JSON key {key} is not an unsigned integer"))
}

/// The same request validation is used for live debugger writes and replay.
pub fn parse_memory_write(json: &str) -> Result<(u64, bool, Vec<u8>), String> {
    let address = memory_integer(json, "address")?.ok_or("memory-write requires address")?;
    let linear = json_bool(json, "linear")?.unwrap_or(false);
    let hex = json_string(json, "hex")?.ok_or("memory-write requires hex")?;
    if hex
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .count()
        > r86::machine::DEBUG_MEMORY_LIMIT * 2
    {
        return Err("memory length exceeds 65536".into());
    }
    Ok((address, linear, parse_hex_bytes(&hex)?))
}

pub fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub fn json_string(json: &str, key: &str) -> Result<Option<String>, String> {
    let Some(value) = json_value(json, key)? else {
        return Ok(None);
    };
    if !value.starts_with('"') {
        return Err(format!("JSON key {key} is not a string"));
    }
    parse_json_string(value).map(Some)
}

pub fn json_u64(json: &str, key: &str) -> Result<Option<u64>, String> {
    let Some(value) = json_value(json, key)? else {
        return Ok(None);
    };
    let number = number_token(value);
    if number.is_empty() || number.starts_with('-') {
        return Err(format!("JSON key {key} is not an unsigned integer"));
    }
    number
        .parse()
        .map(Some)
        .map_err(|_| format!("JSON key {key} is not an unsigned integer"))
}

pub fn json_i64(json: &str, key: &str) -> Result<Option<i64>, String> {
    let Some(value) = json_value(json, key)? else {
        return Ok(None);
    };
    let number = number_token(value);
    if number.is_empty() {
        return Err(format!("JSON key {key} is not an integer"));
    }
    number
        .parse()
        .map(Some)
        .map_err(|_| format!("JSON key {key} is not an integer"))
}

pub fn json_bool(json: &str, key: &str) -> Result<Option<bool>, String> {
    let Some(value) = json_value(json, key)? else {
        return Ok(None);
    };
    if value.starts_with("true") {
        Ok(Some(true))
    } else if value.starts_with("false") {
        Ok(Some(false))
    } else {
        Err(format!("JSON key {key} is not a boolean"))
    }
}

pub fn json_value<'a>(json: &'a str, key: &str) -> Result<Option<&'a str>, String> {
    let pattern = format!("\"{}\"", key);
    let Some(pos) = json.find(&pattern) else {
        return Ok(None);
    };
    let mut rest = &json[pos + pattern.len()..];
    rest = rest.trim_start();
    if !rest.starts_with(':') {
        return Err(format!("JSON key {key} is not followed by ':'"));
    }
    Ok(Some(rest[1..].trim_start()))
}

pub fn number_token(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut end = 0usize;
    if bytes.first() == Some(&b'-') {
        end = 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    &value[..end]
}

pub fn parse_json_string(value: &str) -> Result<String, String> {
    fn code_unit(bytes: &[u8], index: &mut usize) -> Result<u32, String> {
        let digits = bytes
            .get(*index + 1..*index + 5)
            .ok_or("short JSON unicode escape")?;
        let mut unit = 0;
        for &digit in digits {
            // Exactly four ASCII hex digits: no sign, no byte of a multi-byte character.
            unit = unit * 16 + u32::from(hex_nibble(digit).ok_or("invalid JSON unicode escape")?);
        }
        *index += 4;
        Ok(unit)
    }

    let bytes = value.as_bytes();
    if bytes.first() != Some(&b'"') {
        return Err("JSON string does not start with quote".to_string());
    }
    let mut out = String::new();
    let mut index = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return Ok(out),
            b'\\' => {
                index += 1;
                if index >= bytes.len() {
                    return Err("unterminated JSON escape".to_string());
                }
                match bytes[index] {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let mut scalar = code_unit(bytes, &mut index)?;
                        if (0xd800..=0xdbff).contains(&scalar) {
                            if bytes.get(index + 1..index + 3) != Some(b"\\u") {
                                return Err("missing JSON low surrogate".to_string());
                            }
                            index += 2;
                            let low = code_unit(bytes, &mut index)?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return Err("invalid JSON low surrogate".to_string());
                            }
                            scalar = 0x10000 + ((scalar - 0xd800) << 10) + (low - 0xdc00);
                        }
                        let ch = char::from_u32(scalar)
                            .ok_or_else(|| "invalid JSON unicode scalar".to_string())?;
                        out.push(ch);
                    }
                    other => return Err(format!("unsupported JSON escape {other:#x}")),
                }
            }
            byte if byte < 0x20 => return Err("control byte in JSON string".to_string()),
            _ => {
                let ch = value[index..]
                    .chars()
                    .next()
                    .ok_or_else(|| "invalid UTF-8 while parsing JSON string".to_string())?;
                out.push(ch);
                index += ch.len_utf8() - 1;
            }
        }
        index += 1;
    }
    Err("unterminated JSON string".to_string())
}

pub fn json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            ch if ch < ' ' => {
                use std::fmt::Write as _;
                let _ = write!(&mut out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out
}

/// `display-mode` with width, height and optional bits (16): None without
/// width. `add` is read separately.
pub fn parse_display_mode(line: &str) -> Result<Option<(u16, u16, u8)>, String> {
    let Some(width) = json_i64(line, "width")? else {
        return Ok(None);
    };
    let height = json_i64(line, "height")?.ok_or("display-mode needs height with width")?;
    let bits = json_i64(line, "bits")?.unwrap_or(16);
    if !(1..=4096).contains(&width) || !(1..=4096).contains(&height) {
        return Err("display-mode width and height must be 1..4096".into());
    }
    if ![4, 8, 15, 16, 24, 32].contains(&bits) {
        return Err("display-mode bits must be 4, 8, 15, 16, 24 or 32".into());
    }
    Ok(Some((width as u16, height as u16, bits as u8)))
}

pub fn parse_pointer(line: &str) -> Result<(u16, u16, u8, i16), String> {
    let integer = |key: &str, default: Option<i64>| -> Result<i64, String> {
        match json_value(line, key)? {
            Some(raw) => raw
                .split([',', '}'])
                .next()
                .unwrap()
                .trim()
                .parse()
                .map_err(|_| format!("pointer {key} must be an integer")),
            None => default.ok_or_else(|| format!("pointer missing {key}")),
        }
    };
    let x = u16::try_from(integer("x", None)?).map_err(|_| "pointer x must be 0..65535")?;
    let y = u16::try_from(integer("y", None)?).map_err(|_| "pointer y must be 0..65535")?;
    let buttons = integer("buttons", Some(0))?;
    if !(0..=7).contains(&buttons) {
        return Err("pointer buttons must be 0..7".into());
    }
    let wheel = i16::try_from(integer("wheel", Some(0))?)
        .map_err(|_| "pointer wheel must be -32768..32767")?;
    Ok((x, y, buttons as u8, wheel))
}
/// Shared by live input and deterministic replay. Reject malformed values at
/// the protocol boundary instead of narrowing integers into a different input.
pub fn parse_usb_tablet(json: &str) -> Result<(u16, u16, u8, i8), String> {
    let integer = |key: &str| -> Result<Option<i64>, String> {
        let Some(raw) = json_value(json, key)? else {
            return Ok(None);
        };
        raw.split([',', '}'])
            .next()
            .unwrap()
            .trim()
            .parse::<i64>()
            .map(Some)
            .map_err(|_| format!("usb-tablet {key} must be an integer"))
    };
    let coordinate = |key| -> Result<u16, String> {
        let value = integer(key)?.ok_or_else(|| format!("usb-tablet missing {key}"))?;
        if !(0..=i64::from(r86::devices::usb::TABLET_MAX)).contains(&value) {
            return Err(format!("usb-tablet {key} must be 0..32767"));
        }
        Ok(value as u16)
    };
    let x = coordinate("x")?;
    let y = coordinate("y")?;
    let buttons = integer("buttons")?.unwrap_or(0);
    let wheel = integer("wheel")?.unwrap_or(0);
    if !(0..=7).contains(&buttons) || !(-127..=127).contains(&wheel) {
        return Err("usb-tablet buttons must be 0..7 and wheel -127..127".into());
    }
    Ok((x, y, buttons as u8, wheel as i8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn session_profile_subtracts_snapshots_and_idle() {
        let mut machine = Machine::new(0x10000);
        let mut boot = [0u8; 512];
        boot[..3].copy_from_slice(&[0x90, 0xfa, 0xf4]); // NOP; CLI; HLT
        boot[510..].copy_from_slice(&[0x55, 0xaa]);
        machine.load_bootsector(&boot).unwrap();
        machine.enter_bootsector();
        machine.step_once().unwrap(); // Work before the profile is excluded.
        let mut profile = Profile::start(&mut machine);
        profile.engine = "Renamed interpreter label";
        machine.step_once().unwrap();
        machine.step_once().unwrap();
        machine.step_once().unwrap(); // Already halted: idle, not work.
        assert!(machine.advance_halted(17));
        assert_eq!(machine.profile_idle_steps(), 18);
        let report = profile.finish(&mut machine);
        for line in [
            "Steps 1 to 21.",
            "Scheduled steps: 20",
            "Idle steps: 18",
            "Executed/attempted steps: 2",
            "Interpreted steps (interpreter selected): 2",
            "Not applicable: A64 JIT is not selected.",
        ] {
            assert!(report.contains(line), "{report}");
        }
        // A new interval excludes prior idle; a reboot preserves counters.
        let profile = Profile::start(&mut machine);
        machine.reboot().unwrap();
        assert_eq!(machine.profile_idle_steps(), 18);
        assert!(profile.finish(&mut machine).contains("Idle steps: 0"));
    }

    #[test]
    fn json_unicode_escapes_require_four_ascii_hex_digits() {
        for value in [
            r#""\u123é""#,
            r#""\u12€""#,
            r#""\u1😀""#,
            r#""\ué00""#,
            r#""\u+123""#,
            r#""\u12x4""#,
            r#""\u12""#,
            r#""\u"#,
        ] {
            assert!(parse_json_string(value).is_err(), "{value}");
        }
        assert_eq!(
            parse_json_string(r#""\u0000\u0041\u00e9\uFFFF""#).unwrap(),
            "\0Aé\u{ffff}"
        );
    }

    #[test]
    fn json_unicode_surrogates_decode_only_in_pairs() {
        assert_eq!(
            json_string(
                r#"{"text":"é\uD83D\uDE00!\ud800\udc00\udbff\udfff"}"#,
                "text"
            )
            .unwrap(),
            Some("é😀!\u{10000}\u{10ffff}".into())
        );
        for value in [
            r#""\uD800""#,
            r#""\uDBFF""#,
            r#""\uDC00""#,
            r#""\uDFFF""#,
            r#""\uD800x\uDC00""#,
            r#""\uD800\u0041""#,
            r#""\uD800\uD800""#,
            r#""\uD800\u12é""#,
            r#""\uD800\uDC""#,
        ] {
            assert!(parse_json_string(value).is_err(), "{value}");
        }
    }

    #[test]
    fn json_helpers_keep_utf8_slice_boundaries() {
        assert_eq!(
            json_string(r#"{"é":"😀\n漢"}"#, "é").unwrap(),
            Some("😀\n漢".into())
        );
        assert!(json_value(r#"{"é"😀}"#, "é").is_err());
        assert_eq!(number_token("123é"), "123");
        assert_eq!(number_token("-12😀"), "-12");
        assert_eq!(number_token("é"), "");
        assert!(json_u64(r#"{"n":é}"#, "n").is_err());
        assert!(json_i64(r#"{"n":-é}"#, "n").is_err());
        assert!(memory_integer(r#"{"n":12é}"#, "n").is_err());
        assert!(parse_hex_bytes("é").is_err());
    }
}
