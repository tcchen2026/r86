//! Small IPv4 user network. The frontend owns and polls it between guest slices.
//! Only connect and the host resolver run on helpers; their results are messages.
//! Socket readiness is one zero-timeout poll every 10 ms, skipped without sockets.
//! TCP uses a single outstanding segment, bounded queues and no TIME_WAIT.
#[path = "network/packet.rs"]
mod packet;
use crate::machine::Machine;
use packet::*;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const RETRANSMIT: Duration = Duration::from_secs(1);
const MAX_CONNECTIONS: usize = 128;
const MAX_WORKERS: usize = 16;
const MAX_QUEUED: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortForward {
    pub host_port: u16,
    pub guest_port: u16,
}

impl std::str::FromStr for PortForward {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        let mut parts = text.split(':');
        let error = || "net-forward expects tcp:HOSTPORT:GUESTPORT (ports 1..65535)".to_string();
        if parts.next() != Some("tcp") {
            return Err(error());
        }
        let host_port = parts
            .next()
            .ok_or_else(error)?
            .parse::<u16>()
            .map_err(|_| error())?;
        let guest_port = parts
            .next()
            .ok_or_else(error)?
            .parse::<u16>()
            .map_err(|_| error())?;
        if host_port == 0 || guest_port == 0 || parts.next().is_some() {
            return Err(error());
        }
        Ok(Self {
            host_port,
            guest_port,
        })
    }
}

enum Completion {
    Connect(u64, io::Result<TcpStream>),
    Dns(DnsQuery, Vec<Ip>),
}

struct DnsQuery {
    epoch: u64,
    mac: [u8; 6],
    port: u16,
    question: Vec<u8>,
}

pub struct UserNetwork {
    connections: Vec<Connection>,
    listeners: Vec<(TcpListener, u16)>,
    output: VecDeque<Vec<u8>>,
    guest_mac: Option<[u8; 6]>,
    tx: Sender<Completion>,
    rx: Receiver<Completion>,
    workers: usize,
    next_id: u64,
    next_port: u16,
    sequence: u32,
    last_poll: Option<Instant>,
    reset_count: Option<u64>,
    epoch: u64,
}

impl UserNetwork {
    /// Forward listeners bind host loopback only. Call Machine::attach_network
    /// before boot, then poll here on the same core thread each frontend slice.
    pub fn new(forwards: &[PortForward]) -> io::Result<Self> {
        if !cfg!(unix) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "user networking needs a Unix host",
            ));
        }
        let mut listeners = Vec::new();
        for fwd in forwards {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, fwd.host_port))?;
            listener.set_nonblocking(true)?;
            listeners.push((listener, fwd.guest_port));
        }
        let (tx, rx) = mpsc::channel();
        Ok(Self {
            connections: Vec::new(),
            listeners,
            output: VecDeque::new(),
            guest_mac: None,
            tx,
            rx,
            workers: 0,
            next_id: 0,
            next_port: 49152,
            sequence: 0x1234_5678,
            last_poll: None,
            reset_count: None,
            epoch: 0,
        })
    }

    pub fn limit_wait(&self, wait: Duration) -> Duration {
        if !self.connections.is_empty()
            || !self.listeners.is_empty()
            || self.workers != 0
            || !self.output.is_empty()
        {
            wait.min(POLL_INTERVAL)
        } else {
            wait
        }
    }

    pub fn poll(&mut self, machine: &mut Machine) -> io::Result<()> {
        let reset = machine.reset_count();
        if self.reset_count.is_some_and(|old| old != reset) {
            self.connections.clear();
            self.output.clear();
            self.guest_mac = None;
            self.epoch = self.epoch.wrapping_add(1);
        }
        self.reset_count = Some(reset);
        self.deliver(machine);
        while self.output.len() < MAX_QUEUED - 2 {
            let Some(frame) = machine.take_network_transmit() else {
                break;
            };
            self.transmit(&frame);
        }
        self.poll_host(Instant::now())?;
        self.deliver(machine);
        Ok(())
    }

    fn deliver(&mut self, machine: &mut Machine) {
        while let Some(frame) = self.output.front() {
            if !machine.receive_network_frame(frame) {
                break;
            }
            self.output.pop_front();
        }
    }

    fn identity(&mut self) -> (u64, u32) {
        self.next_id = self.next_id.wrapping_add(1);
        self.sequence = self.sequence.wrapping_add(64001);
        (self.next_id, self.sequence)
    }

    fn transmit(&mut self, frame: &[u8]) {
        if !(14..=1514).contains(&frame.len()) {
            return;
        }
        let mac: [u8; 6] = frame[6..12].try_into().unwrap();
        if mac[0] & 1 != 0 || mac == [0; 6] {
            return;
        }
        match u16be(frame, 12) {
            0x0806 => self.arp(mac, &frame[14..]),
            0x0800 => {
                let ip = &frame[14..];
                if ip.len() < 20
                    || ip[0] != 0x45
                    || checksum(&ip[..20], 0) != 0
                    || u16be(ip, 6) & 0xbfff != 0
                    || ip[8] == 0
                {
                    return;
                }
                let len = usize::from(u16be(ip, 2));
                if !(20..=ip.len()).contains(&len) {
                    return;
                }
                let src: Ip = ip[12..16].try_into().unwrap();
                let dst: Ip = ip[16..20].try_into().unwrap();
                let b = &ip[20..len];
                match ip[9] {
                    17 => {
                        if b.len() < 8 || usize::from(u16be(b, 4)) != b.len() {
                            return;
                        }
                        if u16be(b, 6) != 0 && checksum(b, pseudo(src, dst, 17, b.len())) != 0 {
                            return;
                        }
                        if u16be(b, 0) == 68
                            && u16be(b, 2) == 67
                            && (dst == GATEWAY || dst == [255; 4])
                        {
                            self.dhcp(mac, &b[8..]);
                        } else if src == GUEST && dst == GATEWAY && u16be(b, 2) == 53 {
                            self.guest_mac = Some(mac);
                            self.dns(mac, u16be(b, 0), &b[8..]);
                        }
                    }
                    6 if src == GUEST && dst[0] != 0 && dst[0] < 224 && dst != GUEST => {
                        if checksum(b, pseudo(src, dst, 6, b.len())) != 0 {
                            return;
                        }
                        if let Some(tcp) = Tcp::parse(dst, b) {
                            self.guest_mac = Some(mac);
                            self.tcp_input(mac, tcp);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn arp(&mut self, mac: [u8; 6], b: &[u8]) {
        if b.len() < 28 || b[..6] != [0, 1, 8, 0, 6, 4] || b[8..14] != mac {
            return;
        }
        if b[14..18] == GUEST {
            self.guest_mac = Some(mac);
        }
        if u16be(b, 6) != 1 || b[24..28] != GATEWAY {
            return;
        }
        let mut reply = b[..28].to_vec();
        put16(&mut reply, 6, 2);
        reply[18..24].copy_from_slice(&mac);
        reply[24..28].copy_from_slice(&b[14..18]);
        reply[8..14].copy_from_slice(&ROUTER_MAC);
        reply[14..18].copy_from_slice(&GATEWAY);
        self.output.push_back(ethernet(mac, 0x0806, &reply));
    }

    fn dhcp(&mut self, mac: [u8; 6], b: &[u8]) {
        if b.len() < 240
            || b[..3] != [1, 1, 6]
            || b[28..34] != mac
            || b[24..28] != [0; 4]
            || b[236..240] != [99, 130, 83, 99]
        {
            return;
        }
        let mut message = 0;
        let mut requested = None;
        let mut server = None;
        let mut at = 240;
        while at < b.len() {
            let option = b[at];
            at += 1;
            if option == 255 {
                break;
            }
            if option == 0 {
                continue;
            }
            let Some(&len) = b.get(at) else {
                return;
            };
            at += 1;
            let Some(data) = b.get(at..at + usize::from(len)) else {
                return;
            };
            match (option, len) {
                (53, 1) => message = data[0],
                (50, 4) => requested = Some(data),
                (54, 4) => server = Some(data),
                _ => {}
            }
            at += usize::from(len);
        }
        if server.is_some_and(|ip| ip != GATEWAY) {
            return;
        }
        let reply_type = match message {
            1 => 2,
            3 if requested.unwrap_or(&b[12..16]) == GUEST => 5,
            3 => 6,
            8 if b[12..16] == GUEST => 5,
            _ => return,
        };
        self.guest_mac = Some(mac);
        let mut reply = vec![0; 240];
        reply[..3].copy_from_slice(&[2, 1, 6]);
        reply[4..12].copy_from_slice(&b[4..12]);
        reply[12..16].copy_from_slice(&b[12..16]);
        if reply_type != 6 && message != 8 {
            reply[16..20].copy_from_slice(&GUEST);
        }
        reply[20..24].copy_from_slice(&GATEWAY);
        reply[28..44].copy_from_slice(&b[28..44]);
        reply[236..240].copy_from_slice(&b[236..240]);
        reply.extend_from_slice(&[53, 1, reply_type, 54, 4]);
        reply.extend_from_slice(&GATEWAY);
        if reply_type != 6 {
            reply.extend_from_slice(&[1, 4, 255, 255, 255, 0, 3, 4]);
            reply.extend_from_slice(&GATEWAY);
            reply.extend_from_slice(&[6, 4]);
            reply.extend_from_slice(&GATEWAY);
            if message != 8 {
                reply.extend_from_slice(&[51, 4, 0, 1, 0x51, 0x80]);
            }
        }
        reply.push(255);
        reply.resize(300, 0);
        // Broadcast bootstrap/NAK; renewal/INFORM can use the configured IP.
        let dst = if reply_type != 6 && b[12..16] == GUEST {
            GUEST
        } else {
            [255; 4]
        };
        self.output.push_back(udp(
            if dst == GUEST { mac } else { [255; 6] },
            dst,
            67,
            68,
            &reply,
        ));
    }

    fn dns(&mut self, mac: [u8; 6], port: u16, b: &[u8]) {
        if b.len() < 12 || b[2] & 0x80 != 0 || u16be(b, 4) != 1 {
            return;
        }
        let Some((name, end, wire)) = dns_name(b, 12) else {
            return;
        };
        let Some(tail) = b.get(end..end + 4) else {
            return;
        };
        let mut question = b[..12].to_vec();
        question.extend_from_slice(&wire);
        question.extend_from_slice(tail);
        let query = DnsQuery {
            epoch: self.epoch,
            mac,
            port,
            question,
        };
        if b[2] & 0x78 != 0 || u16be(tail, 2) != 1 {
            self.output.push_back(dns_reply(query, &[], 4));
        } else if u16be(tail, 0) != 1 {
            self.output.push_back(dns_reply(query, &[], 0));
        } else if self.workers >= MAX_WORKERS {
            self.output.push_back(dns_reply(query, &[], 3));
        } else {
            let tx = self.tx.clone();
            // Capture owned packet fields only, never a Machine or NIC pointer.
            let fallback = DnsQuery {
                epoch: query.epoch,
                mac,
                port,
                question: query.question.clone(),
            };
            match std::thread::Builder::new()
                .name("r86-dns".into())
                .spawn(move || {
                    let mut ips = Vec::new();
                    if let Ok(addrs) = (name.as_str(), 0).to_socket_addrs() {
                        for addr in addrs {
                            if let SocketAddr::V4(addr) = addr {
                                let ip = addr.ip().octets();
                                if !ips.contains(&ip) {
                                    ips.push(ip);
                                }
                                if ips.len() == 8 {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = tx.send(Completion::Dns(query, ips));
                }) {
                Ok(_) => self.workers += 1,
                Err(_) => self.output.push_back(dns_reply(fallback, &[], 3)),
            }
        }
    }

    fn tcp_input(&mut self, mac: [u8; 6], packet: Tcp<'_>) {
        if let Some(conn) = self
            .connections
            .iter_mut()
            .find(|c| c.key == packet.key && !c.closed)
        {
            conn.input(&packet, &mut self.output);
            return;
        }
        if packet.flags & RST != 0 {
            return;
        }
        if packet.flags & (SYN | ACK) != SYN
            || self.connections.len() >= MAX_CONNECTIONS
            || self.workers >= MAX_WORKERS
        {
            self.output.push_back(reset_reply(mac, &packet));
            return;
        }
        let (id, seq) = self.identity();
        let mut conn = Connection::new(id, packet.key, mac, seq, Phase::Connecting);
        conn.recv_next = packet.seq.wrapping_add(1);
        conn.window = packet.window;
        conn.mss = packet.mss;
        let ip = if packet.key.ip == GATEWAY {
            Ipv4Addr::LOCALHOST
        } else {
            Ipv4Addr::from(packet.key.ip)
        };
        let addr = SocketAddr::from((ip, packet.key.port));
        let tx = self.tx.clone();
        match std::thread::Builder::new()
            .name("r86-connect".into())
            .spawn(move || {
                let socket =
                    TcpStream::connect_timeout(&addr, Duration::from_secs(5)).and_then(|socket| {
                        socket.set_nonblocking(true)?;
                        socket.set_nodelay(true)?;
                        Ok(socket)
                    });
                let _ = tx.send(Completion::Connect(id, socket));
            }) {
            Ok(_) => {
                self.workers += 1;
                self.connections.push(conn);
            }
            Err(_) => self.output.push_back(reset_reply(mac, &packet)),
        }
    }

    fn poll_host(&mut self, now: Instant) -> io::Result<()> {
        while self.output.len() < MAX_QUEUED - 2 {
            let Ok(result) = self.rx.try_recv() else {
                break;
            };
            self.workers -= 1;
            match result {
                Completion::Connect(id, result) => {
                    if let Some(conn) = self
                        .connections
                        .iter_mut()
                        .find(|c| c.id == id && !c.closed)
                    {
                        match result {
                            Ok(socket) => {
                                conn.socket = Some(socket);
                                conn.phase = Phase::SynReceived;
                                conn.send(SYN | ACK, Vec::new(), now, &mut self.output);
                            }
                            Err(_) => conn.reset(&mut self.output),
                        }
                    }
                }
                Completion::Dns(query, ips) if query.epoch == self.epoch => {
                    let code = if ips.is_empty() { 3 } else { 0 };
                    self.output.push_back(dns_reply(query, &ips, code));
                }
                Completion::Dns(..) => {}
            }
        }
        self.connections.retain(|c| !c.closed);
        if self
            .last_poll
            .is_some_and(|last| now.duration_since(last) < POLL_INTERVAL)
        {
            return Ok(());
        }
        self.last_poll = Some(now);
        if self.output.len() >= MAX_QUEUED - 2 {
            return Ok(());
        }
        let ready = readiness(&self.listeners, &self.connections)?;
        let nlisteners = self.listeners.len();
        for (i, conn) in self.connections.iter_mut().enumerate() {
            if self.output.len() >= MAX_QUEUED - 2 {
                break;
            }
            conn.poll(ready[nlisteners + i], now, &mut self.output);
        }
        self.connections.retain(|c| !c.closed);
        for i in 0..nlisteners {
            if self.output.len() >= MAX_QUEUED - 2 || self.connections.len() >= MAX_CONNECTIONS {
                break;
            }
            if ready[i] & READABLE == 0 {
                continue;
            }
            let Some(mac) = self.guest_mac else {
                // A statically configured guest may not have transmitted yet.
                // The listener backlog retains the host connection meanwhile.
                if self.output.is_empty() {
                    let mut arp = [0; 28];
                    arp[..8].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
                    arp[8..14].copy_from_slice(&ROUTER_MAC);
                    arp[14..18].copy_from_slice(&GATEWAY);
                    arp[24..28].copy_from_slice(&GUEST);
                    self.output.push_back(ethernet([255; 6], 0x0806, &arp));
                }
                continue;
            };
            let socket = match self.listeners[i].0.accept() {
                Ok((socket, _)) => socket,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error),
            };
            socket.set_nonblocking(true)?;
            socket.set_nodelay(true)?;
            let key = loop {
                let port = self.next_port;
                self.next_port = if port == u16::MAX { 49152 } else { port + 1 };
                let key = Tuple {
                    ip: GATEWAY,
                    port,
                    guest_port: self.listeners[i].1,
                };
                if !self.connections.iter().any(|c| c.key == key) {
                    break key;
                }
            };
            let (id, seq) = self.identity();
            let mut conn = Connection::new(id, key, mac, seq, Phase::SynSent);
            conn.socket = Some(socket);
            conn.send(SYN, Vec::new(), now, &mut self.output);
            self.connections.push(conn);
        }
        Ok(())
    }
}

// Decode a possibly compressed DNS question into its independent wire form.
// Bound pointer traversal and the expanded name, including label length octets.
fn dns_name(b: &[u8], start: usize) -> Option<(String, usize, Vec<u8>)> {
    let mut at = start;
    let mut end = None;
    let mut labels = Vec::new();
    let mut wire = Vec::new();
    for _ in 0..128 {
        let n = *b.get(at)?;
        if n & 0xc0 == 0xc0 {
            let tail = *b.get(at + 1)?;
            end.get_or_insert(at + 2);
            at = (usize::from(n & 0x3f) << 8) | usize::from(tail);
        } else if n == 0 {
            wire.push(0);
            return Some((labels.join("."), end.unwrap_or(at + 1), wire));
        } else {
            if n > 63 {
                return None;
            }
            let label = b.get(at + 1..at + 1 + usize::from(n))?;
            if !label
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
            {
                return None;
            }
            labels.push(std::str::from_utf8(label).ok()?.to_string());
            wire.push(n);
            wire.extend_from_slice(label);
            if wire.len() >= 255 {
                return None;
            }
            at += 1 + usize::from(n);
        }
    }
    None
}

fn dns_reply(query: DnsQuery, ips: &[Ip], code: u16) -> Vec<u8> {
    let mut b = query.question;
    let flags = 0x8080 | (u16be(&b, 2) & 0x7900) | code;
    put16(&mut b, 2, flags);
    put16(&mut b, 6, ips.len() as u16);
    put16(&mut b, 8, 0);
    put16(&mut b, 10, 0);
    for ip in ips {
        b.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        b.extend_from_slice(ip);
    }
    udp(query.mac, GUEST, 53, query.port, &b)
}

fn reset_reply(mac: [u8; 6], packet: &Tcp<'_>) -> Vec<u8> {
    if packet.flags & ACK != 0 {
        tcp(mac, packet.key, packet.ack, 0, RST, 0, &[])
    } else {
        let ack = packet
            .seq
            .wrapping_add(packet.data.len() as u32)
            .wrapping_add(u32::from(packet.flags & SYN != 0))
            .wrapping_add(u32::from(packet.flags & FIN != 0));
        tcp(mac, packet.key, 0, ack, RST | ACK, 0, &[])
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connecting,
    SynSent,
    SynReceived,
    Established,
}

struct Sent {
    seq: u32,
    flags: u8,
    data: Vec<u8>,
    at: Instant,
    retries: u8,
}
impl Sent {
    fn len(&self) -> u32 {
        self.data.len() as u32 + u32::from(self.flags & SYN != 0) + u32::from(self.flags & FIN != 0)
    }
}

struct Connection {
    id: u64,
    key: Tuple,
    mac: [u8; 6],
    phase: Phase,
    socket: Option<TcpStream>,
    recv_next: u32,
    send_next: u32,
    sent: Option<Sent>,
    window: u16,
    mss: usize,
    to_host: Vec<u8>,
    fin_pending: bool,
    guest_fin: bool,
    host_fin: bool,
    closed: bool,
}

impl Connection {
    fn new(id: u64, key: Tuple, mac: [u8; 6], seq: u32, phase: Phase) -> Self {
        Self {
            id,
            key,
            mac,
            phase,
            socket: None,
            recv_next: 0,
            send_next: seq,
            sent: None,
            window: 0,
            mss: 536,
            to_host: Vec::new(),
            fin_pending: false,
            guest_fin: false,
            host_fin: false,
            closed: false,
        }
    }

    fn receive_window(&self) -> u16 {
        if self.to_host.is_empty() && !self.guest_fin {
            MSS as u16
        } else {
            0
        }
    }

    fn ack(&self, out: &mut VecDeque<Vec<u8>>) {
        out.push_back(tcp(
            self.mac,
            self.key,
            self.send_next,
            self.recv_next,
            ACK,
            self.receive_window(),
            &[],
        ));
    }

    fn send(&mut self, flags: u8, data: Vec<u8>, now: Instant, out: &mut VecDeque<Vec<u8>>) {
        debug_assert!(self.sent.is_none());
        let sent = Sent {
            seq: self.send_next,
            flags,
            data,
            at: now,
            retries: 0,
        };
        out.push_back(tcp(
            self.mac,
            self.key,
            sent.seq,
            self.recv_next,
            flags,
            self.receive_window(),
            &sent.data,
        ));
        self.send_next = self.send_next.wrapping_add(sent.len());
        self.sent = Some(sent);
    }

    fn reset(&mut self, out: &mut VecDeque<Vec<u8>>) {
        out.push_back(tcp(
            self.mac,
            self.key,
            self.send_next,
            self.recv_next,
            RST | ACK,
            0,
            &[],
        ));
        self.abort();
    }

    fn abort(&mut self) {
        if let Some(socket) = self.socket.take() {
            abort_socket(socket);
        }
        self.closed = true;
    }

    fn input(&mut self, p: &Tcp<'_>, out: &mut VecDeque<Vec<u8>>) {
        if p.flags & RST != 0 {
            if (self.phase == Phase::SynSent && p.flags & ACK != 0 && p.ack == self.send_next)
                || (self.phase != Phase::SynSent && p.seq == self.recv_next)
            {
                self.abort();
            }
            return;
        }
        if self.phase == Phase::Connecting {
            return;
        }
        if self.phase == Phase::SynSent {
            if p.flags & (SYN | ACK) == SYN | ACK && p.ack == self.send_next {
                self.recv_next = p.seq.wrapping_add(1);
                self.window = p.window;
                self.mss = p.mss;
                self.sent = None;
                self.phase = Phase::Established;
                self.ack(out);
            }
            return;
        }
        if p.flags & SYN != 0 {
            if self.phase == Phase::SynReceived && p.seq.wrapping_add(1) == self.recv_next {
                let sent = self.sent.as_ref().unwrap();
                out.push_back(tcp(
                    self.mac,
                    self.key,
                    sent.seq,
                    self.recv_next,
                    SYN | ACK,
                    self.receive_window(),
                    &[],
                ));
            } else {
                self.ack(out);
            }
            return;
        }
        if p.flags & ACK == 0 {
            return;
        }
        if self.phase == Phase::SynReceived && (p.seq != self.recv_next || p.ack != self.send_next)
        {
            return;
        }
        if p.ack.wrapping_sub(self.send_next) as i32 > 0 {
            self.ack(out);
            return;
        }
        // Only an in-order segment may update the advertised window or ACK.
        let skip = self.recv_next.wrapping_sub(p.seq) as usize;
        if skip > p.data.len() {
            if !p.data.is_empty() || p.flags & FIN != 0 || p.seq != self.recv_next {
                self.ack(out);
            }
            return;
        }
        if let Some(sent) = &mut self.sent {
            let consumed = p.ack.wrapping_sub(sent.seq);
            if consumed <= sent.len() && consumed != 0 {
                if consumed == sent.len() {
                    self.sent = None;
                } else {
                    sent.data.drain(..consumed as usize);
                    sent.seq = p.ack;
                    sent.at = Instant::now();
                }
            }
        }
        self.window = p.window;
        self.phase = Phase::Established;
        let data = &p.data[skip..];
        if self.guest_fin || !self.to_host.is_empty() {
            if !p.data.is_empty() || p.flags & FIN != 0 {
                self.ack(out);
            }
        } else {
            self.to_host.extend_from_slice(data);
            self.fin_pending |= p.flags & FIN != 0;
            if skip != 0 && data.is_empty() && !self.fin_pending {
                self.ack(out);
            }
        }
        self.finish();
    }

    fn finish(&mut self) {
        if self.guest_fin && self.host_fin && self.sent.is_none() {
            self.closed = true;
        }
    }

    fn poll(&mut self, ready: i16, now: Instant, out: &mut VecDeque<Vec<u8>>) {
        if ready & ERROR != 0 {
            self.reset(out);
            return;
        }
        if self.phase == Phase::Established {
            if ready & WRITABLE != 0 {
                let socket = self.socket.as_mut().unwrap();
                if !self.to_host.is_empty() {
                    match socket.write(&self.to_host) {
                        Ok(0) => {
                            self.reset(out);
                            return;
                        }
                        Ok(n) => {
                            self.to_host.drain(..n);
                            self.recv_next = self.recv_next.wrapping_add(n as u32);
                            if !self.to_host.is_empty() || !self.fin_pending {
                                self.ack(out);
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) => {}
                        Err(_) => {
                            self.reset(out);
                            return;
                        }
                    }
                }
                if self.to_host.is_empty() && self.fin_pending {
                    if self
                        .socket
                        .as_ref()
                        .unwrap()
                        .shutdown(Shutdown::Write)
                        .is_err()
                    {
                        self.reset(out);
                        return;
                    }
                    self.fin_pending = false;
                    self.guest_fin = true;
                    self.recv_next = self.recv_next.wrapping_add(1);
                    self.ack(out);
                }
            }
            if ready & (READABLE | HANGUP) != 0
                && self.sent.is_none()
                && self.window != 0
                && !self.host_fin
            {
                let mut bytes = [0; MSS];
                let len = self.mss.min(usize::from(self.window));
                match self.socket.as_mut().unwrap().read(&mut bytes[..len]) {
                    Ok(0) => {
                        self.host_fin = true;
                        self.send(FIN | ACK, Vec::new(), now, out);
                    }
                    Ok(n) => self.send(ACK | 8, bytes[..n].to_vec(), now, out),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => {
                        self.reset(out);
                        return;
                    }
                }
            }
        }
        if let Some(sent) = &mut self.sent {
            if now.saturating_duration_since(sent.at) >= RETRANSMIT {
                if sent.retries >= 30 {
                    self.reset(out);
                    return;
                }
                sent.retries += 1;
                sent.at = now;
                let len = sent.data.len().min(usize::from(self.window).max(1));
                // A zero-window probe uses one retained byte, never new data.
                let frame = tcp(
                    self.mac,
                    self.key,
                    sent.seq,
                    self.recv_next,
                    sent.flags,
                    if self.to_host.is_empty() && !self.guest_fin {
                        MSS as u16
                    } else {
                        0
                    },
                    &sent.data[..len],
                );
                out.push_back(frame);
            }
        }
        self.finish();
    }
}

const READABLE: i16 = 1;
const WRITABLE: i16 = 4;
const ERROR: i16 = 8;
const HANGUP: i16 = 16;

#[cfg(unix)]
fn readiness(listeners: &[(TcpListener, u16)], connections: &[Connection]) -> io::Result<Vec<i16>> {
    use std::os::fd::AsRawFd;
    let mut fds: Vec<libc::pollfd> = listeners
        .iter()
        .map(|(s, _)| libc::pollfd {
            fd: s.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    for c in connections {
        let mut events = 0;
        if c.phase == Phase::Established {
            if c.sent.is_none() && c.window != 0 && !c.host_fin {
                events |= libc::POLLIN;
            }
            if !c.to_host.is_empty() || c.fin_pending {
                events |= libc::POLLOUT;
            }
        }
        fds.push(libc::pollfd {
            fd: c.socket.as_ref().map_or(-1, AsRawFd::as_raw_fd),
            events,
            revents: 0,
        });
    }
    if fds.iter().any(|f| f.fd >= 0) {
        // poll reads/writes only this initialized array; it retains no pointers.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    Ok(fds
        .into_iter()
        .map(|f| {
            let mut ready = 0;
            if f.revents & libc::POLLIN != 0 {
                ready |= READABLE;
            }
            if f.revents & libc::POLLOUT != 0 {
                ready |= WRITABLE;
            }
            if f.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                ready |= ERROR;
            }
            if f.revents & libc::POLLHUP != 0 {
                ready |= HANGUP;
            }
            ready
        })
        .collect())
}

#[cfg(not(unix))]
fn readiness(_: &[(TcpListener, u16)], _: &[Connection]) -> io::Result<Vec<i16>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "user networking needs a Unix host",
    ))
}

fn abort_socket(socket: TcpStream) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // The kernel copies this C-layout option before returning. Closing with
        // zero linger propagates a guest RST instead of a graceful host FIN.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&linger as *const libc::linger).cast(),
                std::mem::size_of_val(&linger) as libc::socklen_t,
            );
        }
    }
    drop(socket);
}

#[cfg(test)]
#[path = "network/tests.rs"]
mod tests;
