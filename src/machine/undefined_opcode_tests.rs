use super::*;
use crate::x86::cpu::{CR4_OSFXSR, TableReg};

fn write32(bus: &mut HostBus, address: usize, value: u32) {
    bus.ram[address..address + 4].copy_from_slice(&value.to_le_bytes());
}

fn user_machine() -> Machine {
    let mut m = Machine::new(0x10000);
    for (index, low, high) in [
        (1, 0x0000_ffff, 0x00cf_9b00),
        (2, 0x0000_ffff, 0x00cf_9300),
        (3, 0x1000_ffff, 0x00cf_fb00),
        (4, 0x0000_ffff, 0x00cf_f300),
        (5, 0x0900_0067, 0x0000_8b00),
    ] {
        write32(&mut m.bus, 0x100 + index * 8, low);
        write32(&mut m.bus, 0x104 + index * 8, high);
    }
    // Ring-3 #UD enters a ring-0 handler on the TSS stack.
    write32(&mut m.bus, 0x430, 0x0008_0800);
    write32(&mut m.bus, 0x434, 0x0000_8e00);
    write32(&mut m.bus, 0x904, 0x3000);
    write32(&mut m.bus, 0x908, 0x10);
    m.bus.ram[0x800..0x802].copy_from_slice(&[0x43, 0xf4]); // inc ebx; hlt
    m.cpu.enter_protected_flat(0x200, 0);
    m.cpu.gdtr = TableReg {
        base: 0x100,
        limit: 0x2f,
    };
    m.cpu.idtr = TableReg {
        base: 0x400,
        limit: 0x7ff,
    };
    m.cpu.tr = 0x28;
    m.cpu.tr_cache = SegmentCache::new(0x28, 0x900, 0x67, 0x8b);
    m.cpu.set_seg_cache(
        SegmentReg::Cs,
        SegmentCache::new(0x1b, 0x1000, u32::MAX, 0xc0fb),
    );
    m.cpu
        .set_seg_cache(SegmentReg::Ss, SegmentCache::flat(0x23, 0xc0f3));
    m.cpu.set_reg(Reg32::Esp, 0x3800);
    m.cpu.set_reg(Reg32::Eax, 0x1234_5678);
    m.cpu.cr4 |= CR4_OSFXSR; // XP enables FXSR even without SSE.
    m
}

#[test]
fn pentium2_undefined_opcodes_deliver_idt_and_keep_bounded_diagnostics() {
    for code in [
        &[0xf3, 0x0f, 0x2c, 0x05, 0x08, 0x18, 0x3e, 0x01][..], // XP's CVTTSS2SI
        &[0x0f, 0xe0, 0x05, 0x00, 0xf0, 0xff, 0xff],           // PAVGB, unmapped operand
        &[0x0f, 0xae, 0x25, 0x00, 0xf0, 0xff, 0xff],           // XSAVE is also absent
        &[0xdb, 0x08],                                         // FISTTP m32int
        &[0xdd, 0x08],                                         // FISTTP m64int
        &[0xdf, 0x08],                                         // FISTTP m16int
    ] {
        let mut m = user_machine();
        m.bus.ram[0x1200..0x1200 + code.len()].copy_from_slice(code);
        let original = m.cpu.clone();
        for count in 1..=2 {
            m.cpu = original.clone();
            assert!(m.run_steps(1).unwrap().stop.is_none());
            assert_eq!(m.cpu.eip, 0x800);
            assert_eq!(m.cpu.cpl(), 0);
            assert_eq!(m.cpu.reg(Reg32::Eax), 0x1234_5678);
            assert_eq!(m.cpu.reg(Reg32::Esp), 0x2fec);
            assert_eq!(&m.bus.ram[0x2fec..0x2ff4], &[0, 2, 0, 0, 0x1b, 0, 0, 0]);
            let record = *m.undefined_opcodes().next().unwrap();
            assert_eq!(
                (record.address, record.eip, record.cs, record.cpl),
                (0x1200, 0x200, 0x1b, 3)
            );
            assert_eq!(record.count, count);
            assert_eq!(record.step, 0);
            assert_eq!(
                &record.bytes[..code.len()],
                &code.iter().copied().map(Some).collect::<Vec<_>>()
            );
            assert_eq!(m.take_undefined_opcode().is_some(), count == 1);
            assert!(m.take_undefined_opcode().is_none());
            assert!(m.run_steps(1).unwrap().stop.is_none());
            assert_eq!(m.cpu.reg(Reg32::Ebx), 1); // The guest handler really ran.
        }
        m.bus.reset(m.step);
        assert_eq!(m.undefined_opcodes().next().unwrap().count, 2);
    }

    // These #UDs are genuine guest faults, but not absent-CPU diagnostics.
    // Reserved selectors must not become VM stops or log noise.
    for code in [
        &[0x0f, 0x0b][..],
        &[0x0f, 0xb9, 0xc0],
        &[0x0f, 0xff],
        &[0xf0, 0x0f, 0x2c],
        &[0x0f, 0xae, 0xe0],
        &[0x66, 0x0f, 0xae, 0x20],
        &[0x8f, 0xc8],
        &[0xfe, 0xd0],
        &[0xff, 0xf8],
        &[0xff, 0xd8],
        &[0xff, 0xe8],
        &[0x0f, 0xba, 0xc0, 0],
        &[0x0f, 0xc7, 0xc0],
        &[0xd9, 0xd1],
        &[0xd9, 0xe2],
        &[0xd9, 0xef],
        &[0xd9, 0x08],
        &[0xda, 0xe0],
        &[0xdb, 0xe5],
        &[0xdb, 0x20],
        &[0xdd, 0xf0],
        &[0xdd, 0x28],
        &[0xde, 0xd8],
        &[0xdf, 0xe1],
    ] {
        let mut m = user_machine();
        m.bus.ram[0x1200..0x1200 + code.len()].copy_from_slice(code);
        assert!(m.run_steps(1).unwrap().stop.is_none(), "{code:02x?}");
        assert_eq!(m.cpu.eip, 0x800, "{code:02x?}");
        assert_eq!(m.undefined_opcodes().count(), 0, "{code:02x?}");
    }

    // FFREE and the P6 FXCH and FCOMP aliases execute; only BSWAP r16, an
    // undefined result, still stops.
    for code in [&[0xdd, 0xc0][..], &[0xdd, 0xc8], &[0xde, 0xd0]] {
        let mut m = user_machine();
        m.bus.ram[0x1200..0x1200 + code.len()].copy_from_slice(code);
        assert!(m.run_steps(1).unwrap().stop.is_none(), "{code:02x?}");
        assert_eq!(m.cpu.eip, 0x202, "{code:02x?}");
        assert_eq!(m.undefined_opcodes().count(), 0);
    }
    let mut m = user_machine();
    m.bus.ram[0x1200..0x1203].copy_from_slice(&[0x66, 0x0f, 0xc8]);
    assert!(m.run_steps(1).unwrap().stop.is_some());
    assert_eq!(m.undefined_opcodes().count(), 0);

    let mut m = user_machine();
    let original = m.cpu.clone();
    for index in 0..=UNDEFINED_OPCODE_LIMIT {
        m.cpu = original.clone();
        m.cpu.eip = 0x4000 + index as u32 * 16;
        let address = 0x1000 + m.cpu.eip as usize;
        m.bus.ram[address..address + 3].copy_from_slice(&[0x0f, 0xe0, 0xc0]);
        assert!(m.run_steps(1).unwrap().stop.is_none());
    }
    assert_eq!(m.undefined_opcodes().count(), UNDEFINED_OPCODE_LIMIT);
    let mut pending = 0;
    while m.take_undefined_opcode().is_some() {
        pending += 1;
    }
    assert_eq!(pending, 256);
    // Repeats still count after the distinct-address cap, without new events.
    m.cpu = original;
    m.cpu.eip = 0x4000;
    assert!(m.run_steps(1).unwrap().stop.is_none());
    assert_eq!(m.undefined_opcodes().next().unwrap().count, 2);
    assert!(m.take_undefined_opcode().is_none());
}

#[test]
fn icebp_traps_after_instruction_without_gate_dpl_check_or_undefined_record() {
    for code in [&[0xf1][..], &[0x66, 0xf1]] {
        let mut m = user_machine();
        m.bus.ram[0x1200..0x1200 + code.len()].copy_from_slice(code);
        write32(&mut m.bus, 0x408, 0x0008_0800);
        write32(&mut m.bus, 0x40c, 0x0000_8e00); // #DB gate DPL=0, caller CPL=3.
        m.cpu.eflags = 2; // IF=0 cannot mask ICEBP.
        m.cpu.dr[6] = 0xffff_0ff5;
        let report = m.run_steps(1).unwrap();
        assert!(report.stop.is_none());
        assert_eq!((m.cpu.eip, m.cpu.cpl()), (0x800, 0));
        assert_eq!(m.cpu.reg(Reg32::Esp), 0x2fec);
        assert_eq!(
            u32::from_le_bytes(m.bus.ram[0x2fec..0x2fec + 4].try_into().unwrap()),
            0x200 + code.len() as u32
        );
        assert_eq!(
            u32::from_le_bytes(m.bus.ram[0x2ff4..0x2ff4 + 4].try_into().unwrap()),
            2
        ); // no fabricated RF
        assert_eq!(m.cpu.dr[6], 0xffff_0ff5);
        assert_eq!(m.undefined_opcodes().count(), 0);
    }
    // A non-present #DB gate produces #NP(IDT[1]|EXT), not a DPL #GP.
    let mut m = user_machine();
    m.bus.ram[0x1200] = 0xf1;
    write32(&mut m.bus, 0x408, 0x0008_0800);
    write32(&mut m.bus, 0x40c, 0x0000_0e00);
    write32(&mut m.bus, 0x458, 0x0008_0880);
    write32(&mut m.bus, 0x45c, 0x0000_8e00);
    assert!(m.run_steps(1).unwrap().stop.is_none());
    assert_eq!(m.cpu.eip, 0x880);
    assert_eq!(
        u32::from_le_bytes(m.bus.ram[0x2fe8..0x2fe8 + 4].try_into().unwrap()),
        0x0b
    );
    assert_eq!(
        u32::from_le_bytes(m.bus.ram[0x2fec..0x2fec + 4].try_into().unwrap()),
        0x201
    );
    assert_eq!(m.undefined_opcodes().count(), 0);
}
