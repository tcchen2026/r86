use super::*;
use crate::devices::ne2000::MAC;
use std::thread;

fn frame(ip: Ip, protocol: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = ipv4(ROUTER_MAC, GUEST, ip, protocol, payload);
    f[6..12].copy_from_slice(&MAC);
    f
}
fn guest_tcp(key: Tuple, seq: u32, ack: u32, flags: u8, window: u16, data: &[u8]) -> Vec<u8> {
    let header = if flags & SYN != 0 { 24 } else { 20 };
    let mut t = vec![0; header + data.len()];
    put16(&mut t, 0, key.guest_port);
    put16(&mut t, 2, key.port);
    put32(&mut t, 4, seq);
    put32(&mut t, 8, ack);
    t[12] = (header as u8 / 4) << 4;
    t[13] = flags;
    put16(&mut t, 14, window);
    if header == 24 {
        t[20..24].copy_from_slice(&[2, 4, 5, 0xb4]);
    }
    t[header..].copy_from_slice(data);
    let sum = checksum(&t, pseudo(GUEST, key.ip, 6, t.len()));
    put16(&mut t, 16, sum);
    frame(key.ip, 6, &t)
}
fn guest_udp(src: Ip, dst: Ip, sport: u16, dport: u16, data: &[u8]) -> Vec<u8> {
    let mut u = vec![0; 8 + data.len()];
    put16(&mut u, 0, sport);
    put16(&mut u, 2, dport);
    put16(&mut u, 4, (8 + data.len()) as u16);
    u[8..].copy_from_slice(data);
    let sum = checksum(&u, pseudo(src, dst, 17, u.len()));
    put16(&mut u, 6, sum);
    let mut f = ipv4(ROUTER_MAC, src, dst, 17, &u);
    f[6..12].copy_from_slice(&MAC);
    f
}
fn validate(frame: &[u8]) {
    assert_eq!(u16be(frame, 12), 0x0800);
    assert_eq!(checksum(&frame[14..34], 0), 0);
    assert_eq!(usize::from(u16be(frame, 16)), frame.len() - 14);
    let src = frame[26..30].try_into().unwrap();
    let dst = frame[30..34].try_into().unwrap();
    assert_eq!(
        checksum(&frame[34..], pseudo(src, dst, frame[23], frame.len() - 34)),
        0
    );
}
fn next(net: &mut UserNetwork) -> Vec<u8> {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        net.poll_host(Instant::now()).unwrap();
        if let Some(f) = net.output.pop_front() {
            validate(&f);
            return f;
        }
        assert!(Instant::now() < until, "network reply timed out");
        thread::sleep(POLL_INTERVAL);
    }
}
fn tcp_part(f: &[u8]) -> Tcp<'_> {
    Tcp::parse(f[26..30].try_into().unwrap(), &f[34..]).unwrap()
}
fn local_listener() -> TcpListener {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    l.set_nonblocking(true).unwrap();
    l
}
fn accept(l: &TcpListener) -> TcpStream {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        match l.accept() {
            Ok((s, _)) => {
                s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                return s;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until);
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => panic!("{e}"),
        }
    }
}
fn connect(
    net: &mut UserNetwork,
    listener: &TcpListener,
    seq: u32,
    window: u16,
) -> (Tuple, u32, TcpStream) {
    let key = Tuple {
        ip: GATEWAY,
        port: listener.local_addr().unwrap().port(),
        guest_port: 12345,
    };
    net.transmit(&guest_tcp(key, seq, 0, SYN, window, &[]));
    assert!(
        net.output.is_empty(),
        "SYN-ACK must wait for connect result"
    );
    let syn = next(net);
    let syn = tcp_part(&syn);
    assert_eq!(syn.flags, SYN | ACK);
    assert_eq!(syn.ack, seq.wrapping_add(1));
    assert_eq!(syn.mss, 1460);
    let ack = syn.seq.wrapping_add(1);
    net.transmit(&guest_tcp(key, seq.wrapping_add(1), ack, ACK, window, &[]));
    (key, ack, accept(listener))
}

#[test]
fn network_arp_dhcp_dns_and_bad_packets() {
    let mut net = UserNetwork::new(&[]).unwrap();
    let mut arp = [0; 28];
    arp[..8].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
    arp[8..14].copy_from_slice(&MAC);
    arp[14..18].copy_from_slice(&GUEST);
    arp[24..28].copy_from_slice(&GATEWAY);
    let mut f = ethernet([255; 6], 0x806, &arp);
    f[6..12].copy_from_slice(&MAC);
    net.transmit(&f);
    let reply = net.output.pop_front().unwrap();
    assert_eq!(u16be(&reply, 20), 2);
    assert_eq!(&reply[22..28], &ROUTER_MAC);
    assert_eq!(&reply[28..32], &GATEWAY);
    assert_eq!(net.guest_mac, Some(MAC));
    let mut dhcp = vec![0; 240];
    dhcp[..3].copy_from_slice(&[1, 1, 6]);
    put32(&mut dhcp, 4, 123);
    dhcp[28..34].copy_from_slice(&MAC);
    dhcp[236..240].copy_from_slice(&[99, 130, 83, 99]);
    dhcp.extend_from_slice(&[53, 1, 1, 255]);
    net.transmit(&guest_udp([0; 4], [255; 4], 68, 67, &dhcp));
    let offer = next(&mut net);
    let offer = &offer[42..];
    assert_eq!(u32be(offer, 4), 123);
    assert_eq!(&offer[16..20], &GUEST);
    assert_eq!(&offer[240..243], &[53, 1, 2]);
    dhcp.truncate(240);
    dhcp.extend_from_slice(&[53, 1, 3, 50, 4]);
    dhcp.extend_from_slice(&GUEST);
    dhcp.extend_from_slice(&[54, 4]);
    dhcp.extend_from_slice(&GATEWAY);
    dhcp.push(255);
    net.transmit(&guest_udp([0; 4], [255; 4], 68, 67, &dhcp));
    let ack = next(&mut net);
    assert_eq!(&ack[282..285], &[53, 1, 5]);
    assert!(ack.windows(6).any(|x| x == [1, 4, 255, 255, 255, 0]));
    assert!(ack.windows(6).any(|x| x == [6, 4, 10, 0, 2, 2]));
    let mut dns = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 9];
    dns.extend_from_slice(b"localhost");
    dns.extend_from_slice(&[0, 0, 1, 0, 1]);
    let valid = guest_udp(GUEST, GATEWAY, 1234, 53, &dns);
    for cut in 0..valid.len() {
        net.transmit(&valid[..cut]);
    }
    assert!(net.output.is_empty());
    assert_eq!(net.workers, 0);
    let mut bad = valid.clone();
    bad[24] ^= 1;
    net.transmit(&bad); // IP checksum
    let mut bad = valid.clone();
    *bad.last_mut().unwrap() ^= 1;
    net.transmit(&bad); // UDP checksum
    assert_eq!(net.workers, 0);
    net.transmit(&valid);
    let reply = next(&mut net);
    let dns = &reply[42..];
    assert_eq!(u16be(dns, 0), 0x1234);
    assert_eq!(u16be(dns, 2) & 15, 0);
    assert!(u16be(dns, 6) >= 1);
    assert_eq!(&dns[dns.len() - 4..], &[127, 0, 0, 1]);
}

#[test]
fn network_tcp_window_retransmit_and_half_close() {
    let mut net = UserNetwork::new(&[]).unwrap();
    let listener = local_listener();
    let (key, mut ack, mut host) = connect(&mut net, &listener, u32::MAX - 2, 3);
    let mut seq = u32::MAX - 1;
    net.transmit(&guest_tcp(key, seq, ack, ACK | 8, 3, b"hello"));
    assert!(net.output.is_empty(), "data ACK waits for socket write");
    let f = next(&mut net);
    assert_eq!(tcp_part(&f).ack, 3);
    seq = 3;
    let mut data = [0; 5];
    host.read_exact(&mut data).unwrap();
    assert_eq!(&data, b"hello");
    // An already acknowledged prefix must not be written twice.
    net.transmit(&guest_tcp(key, u32::MAX - 1, ack, ACK, 3, b"hello"));
    next(&mut net);
    host.write_all(b"abcdef").unwrap();
    let f = next(&mut net);
    let p = tcp_part(&f);
    assert_eq!(p.data, b"abc");
    assert_eq!(p.seq, ack);
    let future = Instant::now() + RETRANSMIT;
    net.poll_host(future).unwrap();
    let retry = net.output.pop_front().unwrap();
    assert_eq!(tcp_part(&retry).data, b"abc");
    assert_eq!(tcp_part(&retry).seq, ack);
    // Consume a partial ACK; retain the unacknowledged tail.
    ack = ack.wrapping_add(1);
    net.transmit(&guest_tcp(key, seq, ack, ACK, 0, &[]));
    net.poll_host(future + RETRANSMIT).unwrap();
    let probe = net.output.pop_front().unwrap();
    assert_eq!(tcp_part(&probe).seq, ack);
    assert_eq!(tcp_part(&probe).data, b"b");
    ack = ack.wrapping_add(2);
    net.transmit(&guest_tcp(key, seq, ack, ACK, 2, &[]));
    net.last_poll = None;
    let f = next(&mut net);
    let p = tcp_part(&f);
    assert_eq!(p.data, b"de");
    ack = ack.wrapping_add(2);
    net.transmit(&guest_tcp(key, seq, ack, ACK, 2, &[]));
    let f = next(&mut net);
    assert_eq!(tcp_part(&f).data, b"f");
    ack = ack.wrapping_add(1);
    net.transmit(&guest_tcp(key, seq, ack, ACK | FIN, 2, &[]));
    let f = next(&mut net);
    seq += 1;
    assert_eq!(tcp_part(&f).ack, seq);
    assert_eq!(host.read(&mut data).unwrap(), 0);
    // Guest half-close still permits a final host response.
    host.write_all(b"!").unwrap();
    let f = next(&mut net);
    assert_eq!(tcp_part(&f).data, b"!");
    ack = ack.wrapping_add(1);
    net.transmit(&guest_tcp(key, seq, ack, ACK, 2, &[]));
    host.shutdown(Shutdown::Write).unwrap();
    let f = next(&mut net);
    assert_eq!(tcp_part(&f).flags, FIN | ACK);
    ack = ack.wrapping_add(1);
    net.transmit(&guest_tcp(key, seq, ack, ACK, 2, &[]));
    net.poll_host(Instant::now()).unwrap();
    assert!(net.connections.is_empty());
}

#[test]
fn network_tcp_refused_reset_and_forward() {
    let mut net = UserNetwork::new(&[]).unwrap();
    let l = local_listener();
    let key = Tuple {
        ip: GATEWAY,
        port: l.local_addr().unwrap().port(),
        guest_port: 12345,
    };
    drop(l);
    net.transmit(&guest_tcp(key, 100, 0, SYN, 1460, &[]));
    let f = next(&mut net);
    assert_eq!(tcp_part(&f).flags, RST | ACK);
    assert_eq!(tcp_part(&f).ack, 101);
    let l = local_listener();
    let (key, ack, mut host) = connect(&mut net, &l, 100, 1460);
    net.transmit(&guest_tcp(key, 101, ack, RST | ACK, 0, &[]));
    assert!(net.connections[0].closed);
    assert!(host.read(&mut [0; 1]).is_err());
    net.poll_host(Instant::now()).unwrap();
    let (key, _, host) = connect(&mut net, &l, 100, 1460);
    abort_socket(host);
    let f = next(&mut net);
    assert_eq!(tcp_part(&f).flags, RST | ACK);
    assert_eq!(u16be(&f, 34), key.port);
    let listener = local_listener();
    let address = listener.local_addr().unwrap();
    net.listeners.push((listener, 80));
    net.guest_mac = Some(MAC);
    let mut host = TcpStream::connect(address).unwrap();
    host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let f = next(&mut net);
    let p = tcp_part(&f);
    assert_eq!(p.flags, SYN);
    assert_eq!(u16be(&f, 36), 80);
    let key = Tuple {
        ip: GATEWAY,
        port: u16be(&f, 34),
        guest_port: 80,
    };
    let mut ack = p.seq.wrapping_add(1);
    net.transmit(&guest_tcp(key, 77, ack, SYN | ACK, 1460, &[]));
    let f = next(&mut net);
    assert_eq!(tcp_part(&f).ack, 78);
    host.write_all(b"GET").unwrap();
    let f = next(&mut net);
    assert_eq!(tcp_part(&f).data, b"GET");
    ack = ack.wrapping_add(3);
    net.transmit(&guest_tcp(key, 78, ack, ACK, 1460, b"OK"));
    next(&mut net);
    let mut reply = [0; 2];
    host.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"OK");
}
