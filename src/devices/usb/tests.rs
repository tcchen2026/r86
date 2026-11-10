use super::{
    Uhci,
    tablet::{IN, OUT, Reply, SETUP, Tablet},
};
use crate::devices::TICKS_PER_SECOND;
use alloc::{vec, vec::Vec};

const ACTIVE: u32 = 1 << 23;
const IOC: u32 = 1 << 24;
const SPD: u32 = 1 << 29;
fn put(ram: &mut [u8], addr: usize, value: u32) {
    ram[addr..addr + 4].copy_from_slice(&value.to_le_bytes());
}
fn get(ram: &[u8], addr: usize) -> u32 {
    u32::from_le_bytes(ram[addr..addr + 4].try_into().unwrap())
}
fn td(ram: &mut [u8], addr: usize, link: u32, control: u32, pid: u8, ep: u8, len: usize, buf: u32) {
    for (i, value) in [
        link,
        control | 0x7ff,
        u32::from(pid) | (u32::from(ep) << 15) | (((len.wrapping_sub(1) as u32) & 0x7ff) << 21),
        buf,
    ]
    .into_iter()
    .enumerate()
    {
        put(ram, addr + 4 * i, value);
    }
}
fn fixture() -> (Uhci, Vec<u8>) {
    let mut h = Uhci::default();
    let mut ram = vec![0; 0x10000];
    for i in 0..1024 {
        put(&mut ram, 0x1000 + 4 * i, 0x2002);
    }
    put(&mut ram, 0x2000, 1);
    put(&mut ram, 0x2004, 1);
    h.write(8, 4, 0x1000, 0);
    h.write(16, 2, 0xe, 0);
    h.write(0, 2, 1, 0);
    h.advance(0, &mut ram, true, |_, _| {});
    (h, ram)
}
fn frame(h: &mut Uhci, ram: &mut [u8]) {
    let now = h.next_event();
    assert_ne!(now, u64::MAX);
    h.advance(now, ram, true, |_, _| {});
}
fn control(h: &mut Uhci, ram: &mut [u8], setup: [u8; 8], address: u8) -> Vec<u8> {
    ram[0x4000..0x4008].copy_from_slice(&setup);
    td(ram, 0x3000, 0x3024, ACTIVE | (3 << 27), SETUP, 0, 8, 0x4000);
    let len = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
    let input = setup[0] & 0x80 != 0;
    let count = len.div_ceil(8);
    for i in 0..count {
        let size = (len - i * 8).min(8);
        td(
            ram,
            0x3020 + 32 * i,
            (0x3040 + 32 * i) as u32 | 4,
            ACTIVE | (3 << 27),
            if input { IN } else { OUT },
            0,
            size,
            (0x4100 + 8 * i) as u32,
        );
    }
    td(
        ram,
        0x3020 + 32 * count,
        1,
        ACTIVE | IOC | (3 << 27),
        if input { OUT } else { IN },
        0,
        0,
        0,
    );
    for i in 0..count + 2 {
        let a = 0x3008 + 32 * i;
        put(ram, a, get(ram, a) | (u32::from(address) << 8));
    }
    put(ram, 0x2004, 0x3000);
    frame(h, ram);
    assert_eq!(get(ram, 0x2004), 1, "control QH did not complete: {h:?}");
    ram[0x4100..0x4100 + len].to_vec()
}
fn configured() -> (Uhci, Vec<u8>) {
    let (mut h, mut ram) = fixture();
    control(&mut h, &mut ram, [0, 9, 1, 0, 0, 0, 0, 0], 0);
    // Consume the initial report, so the next interrupt request will NAK.
    td(&mut ram, 0x3100, 1, ACTIVE, IN, 1, 8, 0x4200);
    put(&mut ram, 0x2004, 0x3100);
    frame(&mut h, &mut ram);
    h.write(2, 2, 0x1f, 0);
    (h, ram)
}
#[test]
fn usb_registers_reset_ports_and_guest_frame_clock() {
    let (mut h, mut ram) = fixture();
    assert_eq!(h.read(16, 2), 0x95); // no software enable-change indication
    assert_eq!(h.read(18, 2), 0x80);
    assert_eq!(h.read(20, 2), 0xff7f); // nonexistent root port stops driver probing
    assert_eq!(h.read(12, 2), 64);
    h.write(6, 2, 123, 0);
    assert_eq!(h.read(6, 2), 0); // running
    h.advance(TICKS_PER_SECOND - 1, &mut ram, true, |_, _| {});
    assert_eq!(h.read(6, 2), 999);
    h.advance(TICKS_PER_SECOND, &mut ram, true, |_, _| {});
    assert_eq!(h.read(6, 2), 1000);
    assert_eq!(h.walks, 1); // empty QH witnessed once; no 1000 descriptor walks
    h.write(0, 2, 0, TICKS_PER_SECOND);
    assert_eq!(h.read(2, 2), 32);
    assert_eq!(h.next_event(), u64::MAX);
    h.write(6, 1, 0xff, 0);
    assert_eq!(h.read(6, 2), 1000);
    h.write(6, 2, 0xffff, 0);
    assert_eq!(h.read(6, 2), 2047);
    h.write(0, 2, 1, TICKS_PER_SECOND);
    frame(&mut h, &mut ram);
    assert_eq!(h.read(6, 2), 0);
    h.write(16, 1, 0, 0);
    assert_eq!(h.read(16, 2), 0x95);
    h.set_connected(false);
    assert_eq!(h.read(16, 2), 0x8a);
    h.write(16, 2, 0xa, 0);
    assert_eq!(h.read(16, 2), 0x80);
    h.set_connected(true);
    assert_eq!(h.read(16, 2), 0x93);
    h.write(16, 2, 0x204, 0);
    assert_eq!(h.read(16, 2) & 0x204, 0x200);
    h.write(0, 2, 2, 0);
    assert_eq!(h.read(0, 2), 0);
    assert_eq!(h.read(2, 2), 32);
    assert_eq!(h.read(12, 1), 64);
    h.write(0, 2, 4, 0);
    assert_eq!(h.read(0, 2), 4);
    h.write(0, 2, 0, 0);
    assert_eq!(h.read(0, 2), 0);
}
#[test]
fn usb_sof_modify_and_bus_master_clock() {
    let (mut h, mut ram) = fixture();
    h.write(12, 1, 0, 0); // first frame already scheduled using default 64
    frame(&mut h, &mut ram);
    let first = TICKS_PER_SECOND.div_ceil(1000);
    assert_eq!(h.read(6, 2), 1);
    let second = ((12000 + 11936) * TICKS_PER_SECOND).div_ceil(12_000_000);
    assert_eq!(h.next_event(), second);
    assert!(second > first);
    h.advance(TICKS_PER_SECOND, &mut ram, false, |_, _| {
        panic!("bus mastering disabled")
    });
    assert_eq!(h.read(6, 2), 1005);
    assert_eq!(h.next_event(), u64::MAX);
}
#[test]
fn usb_control_descriptors_address_configuration_and_hid() {
    let (mut h, mut ram) = fixture();
    let device = control(&mut h, &mut ram, [0x80, 6, 0, 1, 0, 0, 18, 0], 0);
    assert_eq!(&device[..8], &[18, 1, 0x10, 1, 0, 0, 0, 8]);
    let config = control(&mut h, &mut ram, [0x80, 6, 0, 2, 0, 0, 34, 0], 0);
    assert_eq!(&config[27..], &[7, 5, 0x81, 3, 8, 0, 10]);
    control(&mut h, &mut ram, [0, 5, 7, 0, 0, 0, 0, 0], 0);
    control(&mut h, &mut ram, [0, 9, 1, 0, 0, 0, 0, 0], 7);
    control(&mut h, &mut ram, [0xa1, 1, 0, 1, 0, 0, 6, 0], 7);
    h.input(1234, 2345, 5, -3);
    let report = control(&mut h, &mut ram, [0xa1, 1, 0, 1, 0, 0, 6, 0], 7);
    assert_eq!(report, [5, 0xd2, 4, 0x29, 9, 0xfd]);
    control(&mut h, &mut ram, [0x21, 10, 0, 2, 0, 0, 0, 0], 7);
    assert_eq!(
        control(&mut h, &mut ram, [0xa1, 2, 0, 0, 0, 0, 1, 0], 7),
        [2]
    );
    assert_eq!(
        control(&mut h, &mut ram, [0x81, 6, 0, 0x22, 0, 0, 74, 0], 7).len(),
        74
    );
}
#[test]
fn usb_tablet_control_status_and_nonboot_protocol_stall() {
    let mut t = Tablet::default();
    assert_eq!(t.packet(SETUP, 0, &mut [0, 5, 42, 0, 0, 0, 0, 0], 0), Ok(8));
    assert_eq!(t.address, 0);
    assert_eq!(t.packet(IN, 0, &mut [], 0), Ok(0));
    assert_eq!(t.address, 42);
    assert_eq!(
        t.packet(SETUP, 0, &mut [0x21, 11, 1, 0, 0, 0, 0, 0], 0),
        Err(Reply::Stall)
    );
    assert_eq!(t.packet(IN, 0, &mut [], 0), Err(Reply::Stall));
    assert_eq!(
        t.packet(SETUP, 0, &mut [0x80, 6, 0, 1, 0, 0, 8, 0], 0),
        Ok(8)
    );
    let mut data = [0; 8];
    assert_eq!(t.packet(IN, 0, &mut data, 0), Ok(8));
    assert_eq!(data[0], 18);
    assert_eq!(t.packet(OUT, 0, &mut [], 0), Ok(0));
}
#[test]
fn usb_nak_cache_report_short_packet_ioc_and_dma_edit() {
    let (mut h, mut ram) = configured();
    h.write(4, 2, 12, 0);
    td(
        &mut ram,
        0x3100,
        1,
        ACTIVE | IOC | SPD | (3 << 27),
        IN,
        1,
        8,
        0x4200,
    );
    put(&mut ram, 0x2004, 0x3100);
    let before = h.walks;
    for _ in 0..1000 {
        frame(&mut h, &mut ram);
    }
    assert_eq!(h.walks, before + 1);
    assert_eq!(
        get(&ram, 0x3104) & (ACTIVE | (3 << 27) | (1 << 19)),
        ACTIVE | (3 << 27) | (1 << 19)
    );
    assert!(!h.irq());
    let oldframe = h.read(6, 2);
    h.input(32767, 100, 1, 2);
    frame(&mut h, &mut ram);
    assert_eq!(h.read(6, 2), (oldframe + 1) & 2047);
    assert_eq!(&ram[0x4200..0x4206], &[1, 255, 127, 100, 0, 2]);
    assert_eq!(get(&ram, 0x3104) & (ACTIVE | (1 << 19) | 0x7ff), 5);
    assert_eq!(get(&ram, 0x2004), 0x3100); // short packet leaves QH pointing at TD
    assert!(h.irq());
    h.write(2, 2, 1, 0);
    assert!(!h.irq());
    frame(&mut h, &mut ram);
    assert!(h.irq()); // even inactive IOC is observed
    h.write(2, 2, 1, 0);
    td(&mut ram, 0x3100, 1, ACTIVE, IN, 1, 8, 0x4200);
    frame(&mut h, &mut ram); // NAK cache rebuilt
    // Guest modifies a previously cached TD without a special RAM write hook.
    put(&mut ram, 0x3104, IOC);
    frame(&mut h, &mut ram);
    assert!(h.irq());
}
#[test]
fn usb_depth_breadth_and_legal_fsbr_loop() {
    for depth in [0, 4] {
        let (mut h, mut ram) = fixture();
        ram[0x4000..0x4008].copy_from_slice(&[0, 9, 1, 0, 0, 0, 0, 0]);
        td(
            &mut ram,
            0x3000,
            0x3020 | depth,
            ACTIVE,
            SETUP,
            0,
            8,
            0x4000,
        );
        td(&mut ram, 0x3020, 1, ACTIVE | IOC, IN, 0, 0, 0);
        put(&mut ram, 0x2004, 0x3000);
        frame(&mut h, &mut ram);
        assert_eq!(
            get(&ram, 0x3024) & ACTIVE,
            if depth == 0 { ACTIVE } else { 0 }
        );
        if depth == 0 {
            frame(&mut h, &mut ram);
        }
        assert_eq!(get(&ram, 0x2004), 1);
    }
    let (mut h, mut ram) = configured();
    put(&mut ram, 0x2000, 0x2002); // Windows reclamation link back to QH
    td(&mut ram, 0x3100, 1, ACTIVE | IOC, IN, 1, 8, 0x4200);
    put(&mut ram, 0x2004, 0x3100);
    let walks = h.walks;
    frame(&mut h, &mut ram);
    frame(&mut h, &mut ram);
    assert_eq!(h.walks, walks + 1);
    assert_eq!(h.read(2, 2), 0);
    // A malformed standalone cycle also has a finite per-frame work bound.
    put(&mut ram, 0x1000 + h.read(6, 2) as usize * 4, 0x3200);
    td(&mut ram, 0x3200, 0x3200, 0, IN, 0, 0, 0);
    let old = h.read(6, 2);
    frame(&mut h, &mut ram);
    assert_eq!(h.read(6, 2), old + 1);
}
#[test]
fn usb_hid_idle_and_button_transitions() {
    let (mut h, mut ram) = configured();
    control(&mut h, &mut ram, [0x21, 10, 0, 1, 0, 0, 0, 0], 0);
    td(&mut ram, 0x3100, 1, ACTIVE, IN, 1, 8, 0x4200);
    put(&mut ram, 0x2004, 0x3100);
    for _ in 0..3 {
        frame(&mut h, &mut ram);
        assert_ne!(get(&ram, 0x3104) & ACTIVE, 0);
    }
    frame(&mut h, &mut ram);
    assert_eq!(get(&ram, 0x3104) & ACTIVE, 0);
    h.input(100, 200, 1, 0);
    h.input(110, 220, 1, 0);
    h.input(110, 220, 0, 0);
    for buttons in [1, 0] {
        td(&mut ram, 0x3100, 1, ACTIVE, IN, 1, 8, 0x4200);
        put(&mut ram, 0x2004, 0x3100);
        frame(&mut h, &mut ram);
        assert_eq!(&ram[0x4200..0x4203], &[buttons, 110, 0]);
    }
}
#[test]
fn usb_dma_and_transaction_failures_are_guest_status() {
    for (buffer, pid, len, status) in [
        (0xffff_fff0, IN, 8, 8),
        (0xa0000, IN, 8, 8),
        (0x4000, 0, 8, 16),
        (0x4000, IN, 1281, 16),
    ] {
        let (mut h, mut ram) = fixture();
        td(&mut ram, 0x3000, 1, ACTIVE, pid, 0, len, buffer);
        put(&mut ram, 0x2004, 0x3000);
        frame(&mut h, &mut ram);
        assert_eq!(h.read(2, 2) & (status | 32), status | 32);
        assert_eq!(h.read(0, 2) & 1, 0);
        assert!(h.irq());
    }
    let (mut h, mut ram) = fixture();
    h.write(8, 4, 0xffff_f000, 0);
    frame(&mut h, &mut ram);
    assert_eq!(h.read(2, 2), 40);
    let (mut h, mut ram) = fixture();
    td(
        &mut ram,
        0x3000,
        1,
        ACTIVE | (3 << 27) | IOC,
        IN,
        1,
        8,
        0x4000,
    );
    let token = get(&ram, 0x3008) | (5 << 8);
    put(&mut ram, 0x3008, token);
    put(&mut ram, 0x2004, 0x3000);
    h.write(4, 2, 1, 0);
    for count in [2, 1, 0] {
        frame(&mut h, &mut ram);
        assert_eq!((get(&ram, 0x3004) >> 27) & 3, count);
    }
    assert_eq!(
        get(&ram, 0x3004) & (ACTIVE | (1 << 22) | (1 << 18)),
        (1 << 22) | (1 << 18)
    );
    assert!(h.irq());
    assert_ne!(h.read(2, 2) & 2, 0);
    h.write(2, 2, 3, 0);
    assert!(!h.irq());
    let (mut h, mut ram) = configured();
    td(&mut ram, 0x3100, 1, ACTIVE, OUT, 1, 1, 0x4200);
    put(&mut ram, 0x2004, 0x3100);
    frame(&mut h, &mut ram);
    assert_eq!(get(&ram, 0x3104) & (ACTIVE | (1 << 22)), 1 << 22);
    h.input(1, 2, 0, 0);
    td(&mut ram, 0x3100, 1, ACTIVE, IN, 1, 1, 0x4200);
    put(&mut ram, 0x2004, 0x3100);
    frame(&mut h, &mut ram);
    assert_eq!(get(&ram, 0x3104) & (ACTIVE | (1 << 20)), 1 << 20);
    assert_eq!(h.read(16, 2) & 12, 8);
}

#[test]
fn usb_endpoint_halt_and_suspend_resume_signaling() {
    let (mut h, mut ram) = configured();
    control(&mut h, &mut ram, [2, 3, 0, 0, 0x81, 0, 0, 0], 0);
    assert_eq!(
        control(&mut h, &mut ram, [0x82, 0, 0, 0, 0x81, 0, 2, 0], 0),
        [1, 0]
    );
    td(&mut ram, 0x3100, 1, ACTIVE, IN, 1, 8, 0x4200);
    put(&mut ram, 0x2004, 0x3100);
    frame(&mut h, &mut ram);
    assert_eq!(get(&ram, 0x3104) & (ACTIVE | (1 << 22)), 1 << 22);
    control(&mut h, &mut ram, [2, 1, 0, 0, 0x81, 0, 0, 0], 0);
    assert_eq!(
        control(&mut h, &mut ram, [0x82, 0, 0, 0, 0x81, 0, 2, 0], 0),
        [0, 0]
    );
    control(&mut h, &mut ram, [0, 3, 1, 0, 0, 0, 0, 0], 0); // remote wake enabled
    h.write(2, 2, 31, 0);
    h.write(4, 2, 2, 0);
    h.write(16, 2, 0x1004, 0); // selective suspend
    h.input(10, 20, 0, 0);
    assert_eq!(h.read(16, 2) & 0x1040, 0x1040);
    assert_eq!(h.read(2, 2) & 4, 0); // USBSTS resume is only for global suspend
    h.write(16, 2, 4, 0);
    h.write(0, 2, 8, 0); // global suspend; stop the frame clock
    let frozen = h.read(6, 2);
    h.advance(TICKS_PER_SECOND, &mut ram, true, |_, _| {
        panic!("suspended DMA")
    });
    assert_eq!(h.read(6, 2), frozen);
    h.input(10, 20, 0, 0); // unchanged input is not a wake event
    assert!(!h.irq());
    h.input(20, 30, 1, 0);
    assert!(h.irq());
    assert_eq!(h.read(0, 2) & 24, 24);
    assert_eq!(h.read(16, 2) & 0x40, 0); // enabled, not selectively suspended
    h.write(2, 2, 4, 0);
    assert!(!h.irq());
    h.write(0, 2, 1, TICKS_PER_SECOND);
    td(&mut ram, 0x3100, 1, ACTIVE, IN, 1, 8, 0x4200);
    put(&mut ram, 0x2004, 0x3100);
    frame(&mut h, &mut ram);
    assert_eq!(&ram[0x4200..0x4205], &[0, 10, 0, 20, 0]);
}
