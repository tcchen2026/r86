use super::*;

#[test]
fn sound_capture_reaches_the_end_of_a_cpu_slice() {
    let mut m = Machine::new(0x10000);
    m.bus.ram[..2].copy_from_slice(&[0xeb, 0xfe]);
    m.enable_audio();
    m.run_steps(1000).unwrap();
    let mut pcm = Vec::new();
    m.drain_audio(&mut pcm);
    assert_eq!(pcm.len(), 4); // floor(1000 * 48000 / oscillator), stereo
    m.enable_audio(); // subscribing again starts at the current machine step
    m.run_steps(1000).unwrap();
    m.drain_audio(&mut pcm);
    assert_eq!(pcm.len(), 4);
}

#[test]
fn sound_bus_irq_selection_and_rejected_widths() {
    let mut m = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
    m.bus.io_write_u8(0x22c, 0xf2).unwrap();
    m.bus.io_write_u8(0x22c, 0xf3).unwrap();
    assert!(m.bus.pic.line(5));
    assert_eq!(m.bus.io_read_u16(0x22e).unwrap(), 0xffff);
    assert_eq!(m.bus.io_read_u32(0x22e).unwrap(), 0xffffffff);
    m.bus.io_write_u8(0x224, 0x82).unwrap();
    assert_eq!(m.bus.io_read_u8(0x225).unwrap(), 3);
    m.bus.io_read_u8(0x22e).unwrap();
    assert!(m.bus.pic.line(5));
    m.bus.io_write_u8(0x224, 0x80).unwrap();
    m.bus.io_write_u8(0x225, 4).unwrap(); // reroute pending 16-bit IRQ to 7
    assert!(!m.bus.pic.line(5));
    assert!(m.bus.pic.line(7));
    m.bus.io_write_u8(0x225, 0).unwrap(); // no IRQ jumper means disconnected
    assert!(!m.bus.pic.line(7));
    m.bus.io_write_u8(0x225, 4).unwrap();
    assert!(m.bus.pic.line(7));
    m.bus.io_read_u8(0x22f).unwrap();
    assert!(!m.bus.pic.line(7));
    m.bus.io_write_u8(0x388, 2).unwrap();
    m.bus.io_write_u8(0x389, 255).unwrap();
    m.bus.io_write_u8(0x388, 4).unwrap();
    m.bus.io_write_u8(0x389, 1).unwrap();
    m.bus.current_step = 1536;
    assert_eq!(m.bus.io_read_u8(0x388).unwrap(), 0xc6);
}

#[test]
fn sound_adlib_poll_delays_use_the_machine_clock() {
    let mut m = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
    let write = |m: &mut Machine, reg, value| {
        m.bus.io_write_u8(0x388, reg).unwrap();
        for _ in 0..6 {
            m.bus.io_read_u8(0x388).unwrap();
        }
        m.bus.io_write_u8(0x389, value).unwrap();
        for _ in 0..35 {
            m.bus.io_read_u8(0x388).unwrap();
        }
    };
    write(&mut m, 4, 0x60);
    write(&mut m, 4, 0x80);
    assert_eq!(m.bus.io_read_u8(0x388).unwrap() & 0xe0, 0);
    write(&mut m, 2, 255);
    write(&mut m, 4, 0x21);
    for _ in 0..80 {
        m.bus.io_read_u8(0x388).unwrap();
    }
    assert_eq!(m.bus.io_read_u8(0x388).unwrap() & 0xe0, 0xc0);
    assert!(m.bus.device_ticks() > 80 * TICKS_PER_SECOND / 1_000_000);
    // ISA waits also advance the PIT/DMA scheduler, without inventing retired
    // CPU instructions or pacing the guest with the host audio queue.
    assert_eq!(m.step_count(), 0);
    m.bus.refresh_interrupt_attention(0);
    assert_eq!(
        m.bus.next_attention_step,
        m.bus
            .next_device_event()
            .saturating_sub(m.bus.device_ticks())
    );
}
