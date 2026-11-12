use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub const FRAME_MAGIC: u32 = 0x4636_3852;
pub const FRAME_VERSION: u16 = 1;
pub const FRAME_TYPE_RGBA: u16 = 1;
pub const PIXEL_FORMAT_RGBA: u32 = 1;
pub const FRAME_HEADER_LEN: usize = 32;
/// Largest client binary message (an upload chunk or remote-CD reply).
pub const MAX_BINARY_FRAME: u64 = 1 << 20;

#[derive(Debug, Eq, PartialEq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

pub struct WebSocket {
    stream: TcpStream,
    write_lock: Arc<Mutex<()>>,
}

impl WebSocket {
    pub fn reader(&self) -> Result<Self, String> {
        Ok(Self {
            stream: self.stream.try_clone().map_err(|e| e.to_string())?,
            write_lock: self.write_lock.clone(),
        })
    }

    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    // HTTP and WebSocket share a port. Only the embedded page is served; no
    // guest or host files are exposed by URL. Each connection has its own I/O
    // worker so a slow reader never holds the machine thread.
    pub fn accept(mut stream: TcpStream) -> Result<Option<Self>, String> {
        stream
            .set_nodelay(true)
            .map_err(|err| format!("failed to set TCP_NODELAY: {err}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let request = read_http_request(&mut stream)?;
        let mut line = request.lines().next().unwrap_or("").split_whitespace();
        let method = line.next().unwrap_or("");
        let path = line.next().unwrap_or("").split('?').next().unwrap_or("");
        if method != "GET" || line.next() != Some("HTTP/1.1") {
            http_reply(
                &mut stream,
                "400 Bad Request",
                "text/plain",
                b"Expected HTTP/1.1 GET\n",
            )?;
            return Ok(None);
        }
        let upgrade = header_value(&request, "upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        if !upgrade {
            let (status, mime, body): (_, _, &[u8]) = match path {
                "/" | "/ws_ui.html" => (
                    "200 OK",
                    "text/html; charset=utf-8",
                    include_bytes!("../../web/ws_ui.html"),
                ),
                "/favicon.ico" => ("204 No Content", "image/x-icon", b""),
                _ => ("404 Not Found", "text/plain", b"Not found\n"),
            };
            http_reply(&mut stream, status, mime, body)?;
            return Ok(None);
        }
        if !matches!(path, "/" | "/ws")
            || header_value(&request, "sec-websocket-version") != Some("13")
        {
            http_reply(
                &mut stream,
                "400 Bad Request",
                "text/plain",
                b"Expected WebSocket version 13 on /ws\n",
            )?;
            return Ok(None);
        }
        let key = header_value(&request, "sec-websocket-key")
            .ok_or_else(|| "websocket request missing Sec-WebSocket-Key".to_string())?;
        let accept = websocket_accept_value(key.trim());
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             \r\n"
        );
        stream
            .write_all(response.as_bytes())
            .map_err(|err| format!("failed to write websocket handshake: {err}"))?;
        stream.set_read_timeout(None).map_err(|e| e.to_string())?;
        Ok(Some(Self {
            stream,
            write_lock: Arc::new(Mutex::new(())),
        }))
    }

    pub fn read_message(&mut self) -> Result<Message, String> {
        // Browsers split large messages into a first frame and continuation
        // frames; control frames may come between them.
        let mut message: Option<(u8, Vec<u8>)> = None;
        loop {
            let mut header = [0u8; 2];
            self.stream
                .read_exact(&mut header)
                .map_err(|err| format!("failed to read websocket frame header: {err}"))?;
            let fin = (header[0] & 0x80) != 0;
            let opcode = header[0] & 0x0f;
            let masked = (header[1] & 0x80) != 0;
            let mut len = u64::from(header[1] & 0x7f);
            if header[0] & 0x70 != 0 || !masked {
                return Err("invalid client websocket frame flags".to_string());
            }
            if len == 126 {
                let mut ext = [0u8; 2];
                self.stream
                    .read_exact(&mut ext)
                    .map_err(|err| format!("failed to read websocket length16: {err}"))?;
                len = u64::from(u16::from_be_bytes(ext));
            } else if len == 127 {
                let mut ext = [0u8; 8];
                self.stream
                    .read_exact(&mut ext)
                    .map_err(|err| format!("failed to read websocket length64: {err}"))?;
                len = u64::from_be_bytes(ext);
            }
            let kind = match (opcode, &message) {
                (0x0, Some((kind, _))) => *kind,
                (0x0, None) => return Err("websocket continuation without a message".into()),
                (0x1 | 0x2, None) => opcode,
                (0x1 | 0x2, Some(_)) => return Err("websocket message interrupted".into()),
                (0x8..=0xA, _) if fin && len <= 125 => opcode,
                (0x8..=0xA, _) => return Err("invalid websocket control frame".into()),
                (other, _) => return Err(format!("unsupported websocket opcode {other:#x}")),
            };
            // Binary messages carry upload chunks or remote-CD read replies.
            let limit = match kind {
                0x2 => MAX_BINARY_FRAME,
                0x1 => 64 * 1024,
                _ => 125,
            };
            let received = message.as_ref().map_or(0, |(_, data)| data.len() as u64);
            if received + len > limit {
                return Err(format!(
                    "websocket message too large: {} bytes",
                    received + len
                ));
            }
            let mut mask = [0u8; 4];
            self.stream
                .read_exact(&mut mask)
                .map_err(|err| format!("failed to read websocket mask: {err}"))?;
            let len = usize::try_from(len).map_err(|_| "websocket length overflow".to_string())?;
            let mut payload = vec![0u8; len];
            self.stream
                .read_exact(&mut payload)
                .map_err(|err| format!("failed to read websocket payload: {err}"))?;
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index & 3];
            }
            match opcode {
                0x8 => {
                    let _ = self.send_frame(0x8, &[]);
                    return Ok(Message::Close);
                }
                0x9 => self.send_frame(0xA, &payload)?,
                0xA => {}
                _ => {
                    let data = match message.take() {
                        Some((_, mut data)) => {
                            data.extend_from_slice(&payload);
                            data
                        }
                        None => payload,
                    };
                    if !fin {
                        message = Some((kind, data));
                        continue;
                    }
                    return match kind {
                        0x1 => String::from_utf8(data)
                            .map(Message::Text)
                            .map_err(|err| format!("websocket text was not UTF-8: {err}")),
                        _ => Ok(Message::Binary(data)),
                    };
                }
            }
        }
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), String> {
        self.send_frame(0x1, text.as_bytes())
    }

    pub fn send_binary(&mut self, payload: &[u8]) -> Result<(), String> {
        self.send_frame(0x2, payload)
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        // The reader replies to ping/close while the writer may be sending a
        // large framebuffer. Keep entire frames contiguous on the TCP stream.
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| "websocket writer panicked")?;
        let mut header = Vec::with_capacity(14);
        header.push(0x80 | (opcode & 0x0f));
        if payload.len() < 126 {
            header.push(payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            header.push(126);
            header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            header.push(127);
            header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        self.stream
            .write_all(&header)
            .and_then(|_| self.stream.write_all(payload))
            .map_err(|err| format!("failed to write websocket frame: {err}"))
    }
}

pub fn encode_rgba_frame(
    step: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixels: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + pixels.len());
    out.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    out.extend_from_slice(&FRAME_VERSION.to_le_bytes());
    out.extend_from_slice(&FRAME_TYPE_RGBA.to_le_bytes());
    out.extend_from_slice(&step.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&stride.to_le_bytes());
    out.extend_from_slice(&PIXEL_FORMAT_RGBA.to_le_bytes());
    out.extend_from_slice(pixels);
    out
}

pub fn frame_hash_hex(width: u32, height: u32, format: u32, pixels: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(12 + pixels.len());
    bytes.extend_from_slice(&width.to_le_bytes());
    bytes.extend_from_slice(&height.to_le_bytes());
    bytes.extend_from_slice(&format.to_le_bytes());
    bytes.extend_from_slice(pixels);
    sha256_hex(&bytes)
}

pub fn websocket_accept_value(key: &str) -> String {
    let mut bytes = Vec::with_capacity(key.len() + WS_GUID.len());
    bytes.extend_from_slice(key.as_bytes());
    bytes.extend_from_slice(WS_GUID.as_bytes());
    let mut sha1 = Sha1::new();
    sha1.update(&bytes);
    base64_encode(&sha1.digest())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha256_digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn read_http_request(stream: &mut TcpStream) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut tmp = [0u8; 512];
    loop {
        let read = stream
            .read(&mut tmp)
            .map_err(|err| format!("failed to read websocket handshake: {err}"))?;
        if read == 0 {
            return Err("websocket handshake ended before headers".to_string());
        }
        bytes.extend_from_slice(&tmp[..read]);
        if bytes.len() > 16 * 1024 {
            return Err("websocket handshake exceeds 16 KiB".to_string());
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(bytes).map_err(|err| format!("websocket handshake was not UTF-8: {err}"))
}

fn http_reply(stream: &mut TcpStream, status: &str, mime: &str, body: &[u8]) -> Result<(), String> {
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len())
        .and_then(|_| stream.write_all(body)).map_err(|e| format!("HTTP reply: {e}"))
}

fn header_value<'a>(request: &'a str, wanted: &str) -> Option<&'a str> {
    for line in request.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(wanted) {
            return Some(value.trim());
        }
    }
    None
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// SHA-1 (FIPS 180-4), fed incrementally: the WebSocket handshake and the
/// names of saved uploads (media.rs).
#[derive(Clone)]
pub struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    used: usize,
    bytes: u64,
}

impl Sha1 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6745_2301,
                0xefcd_ab89,
                0x98ba_dcfe,
                0x1032_5476,
                0xc3d2_e1f0,
            ],
            block: [0; 64],
            used: 0,
            bytes: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.bytes += data.len() as u64;
        while !data.is_empty() {
            let n = (64 - self.used).min(data.len());
            self.block[self.used..self.used + n].copy_from_slice(&data[..n]);
            self.used += n;
            data = &data[n..];
            if self.used == 64 {
                self.compress();
                self.used = 0;
            }
        }
    }

    pub fn digest(&self) -> [u8; 20] {
        let mut last = self.clone();
        let bits = self.bytes * 8;
        last.update(&[0x80]);
        while last.used != 56 {
            last.update(&[0]);
        }
        last.update(&bits.to_be_bytes());
        let mut out = [0; 20];
        for (bytes, word) in out.chunks_exact_mut(4).zip(last.state) {
            bytes.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self) {
        let mut w = [0u32; 80];
        for (i, word) in self.block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((b & c) | (!b & d), 0x5a82_7999),
                1 => (b ^ c ^ d, 0x6ed9_eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t.wrapping_add(word));
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e]) {
            *state = state.wrapping_add(value);
        }
    }
}

fn sha256_digest(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut h = [
        0x6a09_e667u32,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let mut msg = input.to_vec();
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (index, word) in w[..16].iter_mut().enumerate() {
            let at = index * 4;
            *word = u32::from_be_bytes([chunk[at], chunk[at + 1], chunk[at + 2], chunk[at + 3]]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (index, word) in h.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_messages_are_reassembled_around_control_frames() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut socket = WebSocket {
            stream: listener.accept().unwrap().0,
            write_lock: Arc::new(Mutex::new(())),
        };
        // Masked client frames; a zero mask leaves the payload as sent.
        let frame = |head: u8, payload: &[u8]| {
            let mut frame = vec![head, 0x80 | payload.len() as u8, 0, 0, 0, 0];
            frame.extend_from_slice(payload);
            frame
        };
        let mut bytes = frame(0x02, b"abc"); // binary, more to come
        bytes.extend(frame(0x89, b"hi")); // ping between fragments
        bytes.extend(frame(0x80, b"def")); // final continuation
        bytes.extend(frame(0x01, "é".as_bytes()[..1].as_ref())); // text split mid-character
        bytes.extend(frame(0x80, "é".as_bytes()[1..].as_ref()));
        bytes.extend(frame(0x80, b"x")); // continuation without a message
        client.write_all(&bytes).unwrap();
        assert!(matches!(socket.read_message(), Ok(Message::Binary(data)) if data == b"abcdef"));
        assert!(matches!(socket.read_message(), Ok(Message::Text(text)) if text == "é"));
        assert!(socket.read_message().is_err());
        // The ping was answered with its payload.
        let mut pong = [0u8; 4];
        client.read_exact(&mut pong).unwrap();
        assert_eq!(pong, [0x8A, 2, b'h', b'i']);
    }

    #[test]
    fn sha1_matches_fips_vectors_across_chunk_boundaries() {
        let digest = |data: &[u8], chunk: usize| {
            let mut sha1 = Sha1::new();
            data.chunks(chunk.max(1)).for_each(|part| sha1.update(part));
            sha1.digest()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(digest(b"", 1), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            digest(b"abc", 1),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        let two_blocks = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            digest(two_blocks, 7),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            digest(&million, 4099),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn websocket_accept_matches_rfc_example() {
        assert_eq!(
            websocket_accept_value("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn rgba_frame_header_is_little_endian_and_fixed_size() {
        let frame = encode_rgba_frame(9, 2, 3, 8, &[1, 2, 3, 4]);
        assert_eq!(frame.len(), FRAME_HEADER_LEN + 4);
        assert_eq!(&frame[0..4], b"R86F");
        assert_eq!(u16::from_le_bytes([frame[4], frame[5]]), FRAME_VERSION);
        assert_eq!(u64::from_le_bytes(frame[8..16].try_into().unwrap()), 9);
        assert_eq!(u32::from_le_bytes(frame[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(frame[20..24].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(frame[24..28].try_into().unwrap()), 8);
        assert_eq!(
            u32::from_le_bytes(frame[28..32].try_into().unwrap()),
            PIXEL_FORMAT_RGBA
        );
        assert_eq!(&frame[FRAME_HEADER_LEN..], &[1, 2, 3, 4]);
    }
}
