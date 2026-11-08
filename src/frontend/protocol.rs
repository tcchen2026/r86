//! JSON and frame helpers shared by the WebSocket console (r86-run and the
//! handheld) and r86-run's command line: field parsing for the small,
//! fixed command schema in script/ws_cli.py, status/stop/frame objects, and
//! a streaming byte matcher for serial waits.

use r86::devices::{Ps2ButtonState, RenderMode};
use r86::machine::{GuestStepStatus, Machine};
use r86::x86::Reg32;
use r86::x86::cpu::SegmentReg;

use super::ws;

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
        "{{\"type\":\"status\",\"ok\":{},\"step\":{},\"clock\":\"{}\",\"cpu\":{{\"eip\":{},\"eax\":{},\"ebx\":{},\"ecx\":{},\"edx\":{},\"esi\":{},\"edi\":{},\"ebp\":{},\"esp\":{},\"eflags\":{},\"cr0\":{},\"cr2\":{},\"cr3\":{},\"cr4\":{},\"cs\":{},\"cs_base\":{},\"cs_limit\":{},\"cs_flags\":{},\"idtr_base\":{},\"idtr_limit\":{},\"gdtr_base\":{},\"gdtr_limit\":{},\"halted\":{}}},\"serial_len\":{},\"vga\":{{\"mode\":\"{}\",\"width\":{},\"height\":{},\"bpp\":{},\"stride\":{},\"dirty\":{}}},\"pic\":{{\"mask1\":{},\"mask2\":{},\"isr1\":{},\"isr2\":{},\"base1\":{},\"base2\":{}}},\"rtc\":{{\"irq8\":{},\"a\":{},\"b\":{},\"c\":{},\"next_tick\":{}}},\"ps2\":{{\"irq1\":{},\"irq12\":{},\"kbd_dropped\":{},\"aux_dropped\":{}}},\"fdc\":{{\"irq6\":{},\"dor\":{},\"msr\":{}}},\"reset_requested\":{},\"reset_count\":{},\"powered_off\":{}}}",
        ok,
        machine.step_count(),
        if machine.uses_realtime_clock() {
            "realtime"
        } else {
            "deterministic"
        },
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
        machine.serial_output().len(),
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
        devices.fdc_irq6,
        devices.fdc_dor,
        devices.fdc_status,
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
                        let end = index + 5;
                        if end > bytes.len() {
                            return Err("short JSON unicode escape".to_string());
                        }
                        let digits = &value[index + 1..end];
                        let scalar = u32::from_str_radix(digits, 16)
                            .map_err(|_| "invalid JSON unicode escape".to_string())?;
                        let ch = char::from_u32(scalar)
                            .ok_or_else(|| "invalid JSON unicode scalar".to_string())?;
                        out.push(ch);
                        index = end - 1;
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
