//! Ethernet/IPv4 wire boundaries. Slices below are checked before field reads.
pub(super) type Ip = [u8; 4];
pub(super) const GATEWAY: Ip = [10, 0, 2, 2];
pub(super) const GUEST: Ip = [10, 0, 2, 15];
pub(super) const ROUTER_MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 2];
pub(super) const FIN: u8 = 1;
pub(super) const SYN: u8 = 2;
pub(super) const RST: u8 = 4;
pub(super) const ACK: u8 = 16;
pub(super) const MSS: usize = 1460;

pub(super) fn u16be(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}
pub(super) fn u32be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
pub(super) fn put16(b: &mut [u8], at: usize, n: u16) {
    b[at..at + 2].copy_from_slice(&n.to_be_bytes());
}
pub(super) fn put32(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_be_bytes());
}

pub(super) fn checksum(b: &[u8], mut sum: u32) -> u16 {
    let mut pairs = b.chunks_exact(2);
    for p in &mut pairs {
        sum += u32::from(u16be(p, 0));
    }
    if let Some(&last) = pairs.remainder().first() {
        sum += u32::from(last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

pub(super) fn pseudo(src: Ip, dst: Ip, protocol: u8, len: usize) -> u32 {
    u32::from(u16be(&src, 0))
        + u32::from(u16be(&src, 2))
        + u32::from(u16be(&dst, 0))
        + u32::from(u16be(&dst, 2))
        + u32::from(protocol)
        + len as u32
}

pub(super) fn ethernet(mac: [u8; 6], kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.extend_from_slice(&mac);
    frame.extend_from_slice(&ROUTER_MAC);
    frame.extend_from_slice(&kind.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

pub(super) fn ipv4(mac: [u8; 6], src: Ip, dst: Ip, protocol: u8, payload: &[u8]) -> Vec<u8> {
    let mut ip = vec![0; 20 + payload.len()];
    ip[0] = 0x45;
    let len = ip.len() as u16;
    put16(&mut ip, 2, len);
    put16(&mut ip, 6, 0x4000);
    ip[8] = 64;
    ip[9] = protocol;
    ip[12..16].copy_from_slice(&src);
    ip[16..20].copy_from_slice(&dst);
    let sum = checksum(&ip[..20], 0);
    put16(&mut ip, 10, sum);
    ip[20..].copy_from_slice(payload);
    ethernet(mac, 0x0800, &ip)
}

pub(super) fn udp(mac: [u8; 6], dst: Ip, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let mut udp = vec![0; 8 + payload.len()];
    put16(&mut udp, 0, sport);
    put16(&mut udp, 2, dport);
    let len = udp.len();
    put16(&mut udp, 4, len as u16);
    udp[8..].copy_from_slice(payload);
    let sum = checksum(&udp, pseudo(GATEWAY, dst, 17, len));
    put16(&mut udp, 6, if sum == 0 { 0xffff } else { sum });
    ipv4(mac, GATEWAY, dst, 17, &udp)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Tuple {
    pub ip: Ip,
    pub port: u16,
    pub guest_port: u16,
}

pub(super) struct Tcp<'a> {
    pub key: Tuple,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: usize,
    pub data: &'a [u8],
}

impl<'a> Tcp<'a> {
    pub fn parse(ip: Ip, b: &'a [u8]) -> Option<Self> {
        if b.len() < 20 {
            return None;
        }
        let header = usize::from(b[12] >> 4) * 4;
        if !(20..=b.len()).contains(&header) {
            return None;
        }
        let mut at = 20;
        let mut mss = 536;
        while at < header {
            let kind = b[at];
            if kind == 0 {
                break;
            }
            if kind == 1 {
                at += 1;
                continue;
            }
            let len = usize::from(*b.get(at + 1)?);
            if len < 2 || at + len > header {
                return None;
            }
            if kind == 2 && len == 4 {
                mss = usize::from(u16be(b, at + 2)).clamp(1, MSS);
            }
            at += len;
        }
        Some(Self {
            key: Tuple {
                ip,
                port: u16be(b, 2),
                guest_port: u16be(b, 0),
            },
            seq: u32be(b, 4),
            ack: u32be(b, 8),
            flags: b[13],
            window: u16be(b, 14),
            mss,
            data: &b[header..],
        })
    }
}

pub(super) fn tcp(
    mac: [u8; 6],
    key: Tuple,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    data: &[u8],
) -> Vec<u8> {
    let header = if flags & SYN != 0 { 24 } else { 20 };
    let mut b = vec![0; header + data.len()];
    put16(&mut b, 0, key.port);
    put16(&mut b, 2, key.guest_port);
    put32(&mut b, 4, seq);
    put32(&mut b, 8, ack);
    b[12] = (header as u8 / 4) << 4;
    b[13] = flags;
    put16(&mut b, 14, window);
    if flags & SYN != 0 {
        b[20..24].copy_from_slice(&[2, 4, 5, 0xb4]);
    }
    b[header..].copy_from_slice(data);
    let sum = checksum(&b, pseudo(key.ip, GUEST, 6, b.len()));
    put16(&mut b, 16, sum);
    ipv4(mac, key.ip, GUEST, 6, &b)
}
