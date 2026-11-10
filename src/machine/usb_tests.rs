use super::*;

#[test]
fn usb_pci_decode_irq_route_reset_and_dma_notification() {
    let mut m = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
    let read = |m: &mut Machine, reg: u32| {
        m.bus.io_write_u32(0xcf8, 0x8000_0a00 | reg).unwrap();
        m.bus.io_read_u32(0xcfc).unwrap()
    };
    assert_eq!(read(&mut m, 0), u32::MAX);
    assert!(m.inject_usb_tablet(0, 0, 0, 0).is_err());
    m.attach_usb_tablet();
    assert_eq!(read(&mut m, 0), 0x7020_8086);
    assert_eq!(read(&mut m, 8), 0x0c03_0001);
    assert_eq!(read(&mut m, 4), 0x0280_0000);
    assert_eq!(read(&mut m, 0x3c), 0x0400);
    assert_eq!(m.bus.pic.read(0x4d1) & 8, 8);
    read(&mut m, 0x20);
    m.bus.io_write_u32(0xcfc, u32::MAX).unwrap();
    assert_eq!(read(&mut m, 0x20), 0xffff_ffe1);
    m.bus.io_write_u32(0xcfc, 0xd001).unwrap();
    assert_eq!(m.bus.io_read_u16(0xd010).unwrap(), 0xffff);
    read(&mut m, 4);
    m.bus.io_write_u8(0xcfc, 5).unwrap();
    assert_eq!(m.bus.io_read_u16(0xd010).unwrap(), 0x93);
    read(&mut m, 0x20);
    m.bus.io_write_u32(0xcfc, 0x1_d001).unwrap();
    assert_eq!(m.bus.io_read_u16(0xd010).unwrap(), 0xffff);
    m.bus.io_write_u32(0xcfc, 0xd001).unwrap();
    read(&mut m, 0x3c);
    m.bus.io_write_u16(0xcfc, 0xff0b).unwrap();
    assert_eq!(read(&mut m, 0x3c), 0x040b); // INTD read-only; line writable
    assert_eq!(read(&mut m, 0x60), 0x10);
    assert_eq!(read(&mut m, 0xc0), 0x2000);
    assert!(m.inject_usb_tablet(32768, 0, 0, 0).is_err());
    assert!(m.inject_usb_tablet(0, 0, 8, 0).is_err());
    assert!(m.inject_usb_tablet(0, 0, 0, -128).is_err());
    assert!(m.inject_usb_tablet(32767, 32767, 7, 127).is_ok());
    // Inactive IOC proves the guest clock event reaches PCI INTD/ELCR/PIC.
    m.bus.ram[0x1000..0x1004].copy_from_slice(&0x2000u32.to_le_bytes());
    m.bus.ram[0x2000..0x2004].copy_from_slice(&1u32.to_le_bytes());
    m.bus.ram[0x2004..0x2008].copy_from_slice(&(1u32 << 24).to_le_bytes());
    m.bus.io_write_u32(0xd008, 0x1000).unwrap();
    m.bus.io_write_u16(0xd004, 4).unwrap();
    m.bus.io_write_u16(0xd000, 1).unwrap();
    m.bus
        .refresh_interrupt_attention(TICKS_PER_SECOND.div_ceil(1000));
    assert!(m.bus.pic.line(11));
    m.bus.sync_pci_irq();
    assert!(m.bus.pic.line(11)); // inactive NIC cannot lower USB
    read(&mut m, 0xc0);
    m.bus.io_write_u16(0xcfc, 0).unwrap();
    assert!(!m.bus.pic.line(11));
    m.bus.io_write_u16(0xcfc, 0x2000).unwrap();
    assert!(m.bus.pic.line(11));
    m.attach_network();
    m.bus.io_write_u8(0xc10f, 0x40).unwrap(); // NIC remote DMA completion
    m.bus.io_write_u8(0xc100, 0x0a).unwrap();
    m.bus.io_write_u16(0xd002, 1).unwrap();
    assert!(m.bus.pic.line(11)); // acknowledging USB must preserve NIC level
    m.bus.io_write_u8(0xc107, 0x40).unwrap();
    assert!(!m.bus.pic.line(11));
    // A failed transaction writes its descriptor through the tracked DMA path.
    m.bus.ram[0x1004..0x1008].copy_from_slice(&0x2000u32.to_le_bytes());
    m.bus.ram[0x2004..0x2008].copy_from_slice(&((1u32 << 23) | (1 << 27)).to_le_bytes());
    m.bus.ram[0x2008..0x200c].copy_from_slice(&(0x69u32 | (7 << 21)).to_le_bytes());
    m.bus.ram[0x200c..0x2010].copy_from_slice(&0x3000u32.to_le_bytes());
    let writes = m.bus.tracked_writes.device;
    m.bus
        .refresh_interrupt_attention((TICKS_PER_SECOND * 2).div_ceil(1000));
    assert!(m.bus.tracked_writes.device > writes);
    m.bus.reset(0);
    assert_eq!(read(&mut m, 0), 0x7020_8086);
    assert_eq!(read(&mut m, 4), 0x0280_0000);
    assert_eq!(m.bus.pic.read(0x4d1) & 8, 8);
}
