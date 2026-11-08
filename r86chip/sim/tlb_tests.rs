//! Guest lookup behavior, firmware isolation and shared-x86-walker examples.
//! These tests execute real custom words; no public host API installs entries.
use super::*;

fn word(f: u32, d: u32, a: u32, upper: u32) -> u32 {
    0x2b | d << 7 | f << 12 | a << 15 | upper << 20
}
fn execute(c: &mut Core, m: &mut Memory, instruction: u32) {
    m.write(c.pc, &instruction.to_le_bytes()).unwrap();
    assert_eq!(c.step(m).unwrap(), None, "word={instruction:08x}");
}
fn region(c: &mut Core, m: &mut Memory, base: u32, limit: u32) {
    c.x[1] = base;
    c.x[2] = limit;
    execute(c, m, word(4, 0, 1, 2));
}
fn fill_view(c: &mut Core, m: &mut Memory, linear: u32, page_rights: u32, fetch: bool) {
    c.x[1] = linear;
    c.x[2] = page_rights;
    execute(c, m, word(1, 0, 1, 2 | (u32::from(fetch) << 5)));
}
// A fixture requesting both views emits TWO real native words. There is no
// host table install and no single instruction that merges independent proofs.
fn fill(c: &mut Core, m: &mut Memory, linear: u32, page_rights: u32) {
    fill_view(c, m, linear, page_rights & !4, false);
    fill_view(c, m, linear, page_rights & !3, true);
}
fn lookup(c: &mut Core, m: &mut Memory, linear: u32, mode: u32) -> u32 {
    c.x[1] = linear;
    // Aliasing rs1/rd must use the old linear address before writeback.
    execute(c, m, word(0, 1, 1, mode));
    c.x[1]
}

#[test]
fn guest_tlb_attribution_distinguishes_rights_churn_collisions_and_revocation() {
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    c.tlb_stats = Some(Box::default());
    region(&mut c, &mut m, 0x10000, 0x18000);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 6), u32::MAX);
    fill_view(&mut c, &mut m, 0x4000, 0x10001, false);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 6), u32::MAX);
    fill_view(&mut c, &mut m, 0x4000, 0x10003, false);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 6), 0x10000);
    fill_view(&mut c, &mut m, 0x4000, 0x10003, false);
    // Replacing RW with R revokes W; observation must never union permissions.
    fill_view(&mut c, &mut m, 0x4000, 0x10001, false);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 6), u32::MAX);
    fill_view(&mut c, &mut m, 0x4000, 0x11001, false);
    fill_view(&mut c, &mut m, 0x14000, 0x10003, false);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 2), u32::MAX);
    assert_eq!(lookup(&mut c, &mut m, 0x14001, 1), u32::MAX);
    assert_eq!(lookup(&mut c, &mut m, 0x14000, 14), 0x10000);
    fill_view(&mut c, &mut m, 0x14000, 0x18001, false);
    assert_eq!(lookup(&mut c, &mut m, 0x14000, 2), u32::MAX);
    fill_view(&mut c, &mut m, 0x4000, 0x10001, false);
    c.x[1] = 0x14000;
    execute(&mut c, &mut m, word(2, 0, 1, 0));
    execute(&mut c, &mut m, word(3, 0, 0, 0));
    let s = c.tlb_stats.as_ref().unwrap();
    assert_eq!(s.lookup[0], [0, 0, 0, 1, 1, 1, 0]);
    assert_eq!(s.lookup[1], [1, 0, 0, 0, 1, 0, 2]);
    assert_eq!(s.lookup[3], [1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(s.fill[1], [1, 2, 0, 1, 1, 0]);
    assert_eq!(s.fill[3], [0, 0, 1, 0, 1, 1]);
    assert_eq!(s.same_mapping_rights[1][3], 1);
    assert_eq!(s.same_mapping_rights[3][1], 1);
    assert_eq!(s.collision_rights[1][3], 1);
    assert_eq!(s.invalidation[0][1], 1);
    assert_eq!(s.invalidation[1][0], 1);
    assert_eq!(s.invalidation[2][0], 1);
}

#[test]
fn data_and_execute_proofs_have_independent_capacity_but_shared_invalidation() {
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    region(&mut c, &mut m, 0x10000, 0x18000);
    fill_view(&mut c, &mut m, 0x4000, 0x10003, false);
    fill_view(&mut c, &mut m, 0x4000, 0x11004, true);
    assert_eq!(lookup(&mut c, &mut m, 0x4008, 2), 0x10008);
    assert_eq!(lookup(&mut c, &mut m, 0x4008, 10), 0x11008);
    // Data collision cannot evict a qualified fetch; fetch collision cannot
    // evict data. Capacity and tag widths differ, but view selection is explicit.
    fill_view(&mut c, &mut m, 0x14000, 0x12003, false);
    assert_eq!(lookup(&mut c, &mut m, 0x4008, 2), u32::MAX);
    assert_eq!(lookup(&mut c, &mut m, 0x4008, 10), 0x11008);
    fill_view(&mut c, &mut m, 0x8000, 0x13004, true);
    assert_eq!(lookup(&mut c, &mut m, 0x4008, 10), u32::MAX);
    assert_eq!(lookup(&mut c, &mut m, 0x14008, 14), 0x12008);
    assert_eq!(lookup(&mut c, &mut m, 0x8008, 10), 0x13008);
    assert_eq!(lookup(&mut c, &mut m, 0x8008, 2), u32::MAX);
    c.x[1] = 0x14000;
    execute(&mut c, &mut m, word(2, 0, 1, 0));
    assert_eq!(lookup(&mut c, &mut m, 0x14008, 2), u32::MAX);
    assert_eq!(lookup(&mut c, &mut m, 0x8008, 10), u32::MAX);
    // Refused/zero-rights fills retire only their selected view's old proof.
    fill(&mut c, &mut m, 0x4000, 0x10007);
    fill_view(&mut c, &mut m, 0x4000, 0x18004, true);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 2), 0x10000);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 10), u32::MAX);
    fill_view(&mut c, &mut m, 0x4000, 0x11004, true);
    fill_view(&mut c, &mut m, 0x4000, 0x10000, false);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 2), u32::MAX);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 10), 0x11000);
    for operation in [3, 4, 5] {
        // flush, identical region, changed context
        fill(&mut c, &mut m, 0x4000, 0x10007);
        c.x[1] = if operation == 4 { 0x10000 } else { 1 };
        c.x[2] = if operation == 4 { 0x18000 } else { 0 };
        execute(
            &mut c,
            &mut m,
            match operation {
                3 => word(3, 0, 0, 0),
                _ => word(operation, 0, 1, 2),
            },
        );
        assert_eq!(lookup(&mut c, &mut m, 0x4000, 2), u32::MAX);
        assert_eq!(lookup(&mut c, &mut m, 0x4000, 10), u32::MAX);
    }
}

#[test]
fn guest_memory_attribution_counts_valid_attempts_before_native_faults() {
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    c.tlb_stats = Some(Box::default());
    let fetch: u32 = 0x5b | 3 << 7 | 1 << 15 | 17 << 25; // XLD x3,CS:[x1],byte,X
    c.x[1] = 0;
    m.write(c.pc, &fetch.to_le_bytes()).unwrap();
    assert_eq!(c.step(&mut m).unwrap().unwrap().cause, Cause::GuestService);
    assert_eq!(c.tlb_stats.as_ref().unwrap().memory[2][1], 1);
    c.pc += 4; // harness resumes after the faulting word
    c.x[1] = 1; // PE, supervisor
    execute(&mut c, &mut m, word(5, 0, 1, 0));
    c.x[1] = 0;
    c.x[2] = 0xfff;
    execute(&mut c, &mut m, word(6, 1, 1, (4 << 5) | 2)); // CS execute proof
    // Qualified aperture deliberately exceeds the actual native bus. A table
    // hit is counted even though the following load faults and cannot retire.
    region(&mut c, &mut m, 0x30000, 0x40000);
    fill(&mut c, &mut m, 0, 0x30004);
    c.x[1] = 0;
    c.x[3] = 0xface_b00c;
    let retired = c.retired_steps;
    m.write(c.pc, &fetch.to_le_bytes()).unwrap();
    assert_eq!(c.step(&mut m).unwrap().unwrap().cause, Cause::LoadAccess);
    assert_eq!(c.x[3], 0xface_b00c);
    assert_eq!(c.retired_steps, retired);
    assert_eq!(c.xld_ops, 0);
    assert_eq!(
        c.tlb_stats.as_ref().unwrap().memory[2],
        [1, 1, 0, 0, 0, 0, 0]
    );
    // A reserved XLD width is not an encoding-valid attempt.
    c.pc += 4;
    let before = c.tlb_stats.as_ref().unwrap().json();
    m.write(c.pc, &(fetch | 3 << 12).to_le_bytes()).unwrap();
    assert_eq!(
        c.step(&mut m).unwrap().unwrap().cause,
        Cause::IllegalInstruction
    );
    assert_eq!(c.tlb_stats.as_ref().unwrap().json(), before);
}

#[test]
fn guest_tlb_reset_region_replacement_and_firmware_isolation() {
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    let sentinel = 0x8765_4321u32;
    m.write(0x800, &sentinel.to_le_bytes()).unwrap(); // private firmware data
    fill(&mut c, &mut m, 0x4000, 0x10007);
    assert_eq!(lookup(&mut c, &mut m, 0x4004, 2), u32::MAX);
    region(&mut c, &mut m, 0x10000, 0x18000);
    for page in [0x10000, 0x17000] {
        fill(&mut c, &mut m, 0x4008, page | 7);
        assert_eq!(lookup(&mut c, &mut m, 0x4ffc, 2), page + 0xffc);
        // None of these guest-controlled PTE-like values may point into native
        // firmware, a gap, or beyond the exclusive guest backing limit.
        for forbidden in [0, 0x1000, 0xf000, 0x18000, 0xffff_f000] {
            fill(&mut c, &mut m, 0x4000, forbidden | 7);
            assert_eq!(lookup(&mut c, &mut m, 0x4800, 0), u32::MAX);
            // Invalid fill removed the prior good mapping, rather than leaving
            // a stale hit for the software service to accidentally consume.
            assert_eq!(lookup(&mut c, &mut m, 0x4000, 2), u32::MAX);
            fill(&mut c, &mut m, 0x4000, page | 7);
        }
    }
    assert_eq!(m.read(0x800, 32), sentinel);
    region(&mut c, &mut m, 0x10000, 0x18000); // even identical bounds flush
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 2), u32::MAX);
    fill(&mut c, &mut m, 0x4000, 0x10007);
    region(&mut c, &mut m, 0x18000, 0x18000); // disabled
    fill(&mut c, &mut m, 0x4000, 0x18007);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 2), u32::MAX);
    // The aperture limits the guest probe, not trusted native RV instructions.
    c.x[1] = 0x800;
    execute(&mut c, &mut m, 0x0000_a183); // LW x3,0(x1)
    assert_eq!(c.x[3], sentinel);
}

#[test]
fn guest_tlb_rights_widths_boundaries_and_pure_probes() {
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    // No RAM is actually allocated at these native addresses. Looking them up
    // must not touch a bus, and must not confuse native addresses with guest PA.
    region(&mut c, &mut m, 0x8000_0000, 0x8001_0000);
    c.x[3] = 0x8d5;
    execute(&mut c, &mut m, 0x0001_c00b); // XFLOAD x3
    let initial_flags = (c.flags, c.pending_flags);
    for rights in 0..8 {
        fill(&mut c, &mut m, 0xffff_f000, 0x8000_0000 | rights);
        for intent in 0..4 {
            let required = [1, 2, 4, 3][intent as usize];
            for size in 0..3 {
                for offset in [0, 1, 2, 3, 0x7ff, 0xffc, 0xffd, 0xffe, 0xfff] {
                    let allowed = rights & required == required && offset % (1 << size) == 0;
                    let expected = if allowed {
                        0x8000_0000 + offset
                    } else {
                        u32::MAX
                    };
                    assert_eq!(
                        lookup(&mut c, &mut m, 0xffff_f000 + offset, intent * 4 + size),
                        expected
                    );
                    assert_eq!((c.flags, c.pending_flags), initial_flags);
                }
            }
        }
    }
    // Lowest native page and highest representable exclusive bound work too.
    region(&mut c, &mut m, 0, 0xffff_f000);
    fill(&mut c, &mut m, 0, 7);
    assert_eq!(lookup(&mut c, &mut m, 0, 2), 0);
    fill(&mut c, &mut m, 0, 0xffff_e007);
    assert_eq!(lookup(&mut c, &mut m, 0xfff, 0), 0xffff_efff);
    assert_eq!(c.x[0], 0);
}

#[test]
fn guest_tlb_collisions_invalidation_banks_and_watch_aliases() {
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    region(&mut c, &mut m, 0x10000, 0x20000);
    for n in 0..16 {
        fill(&mut c, &mut m, n << 12, 0x10007);
    }
    for n in 0..16 {
        assert_eq!(lookup(&mut c, &mut m, n << 12, 2), 0x10000);
    }
    fill(&mut c, &mut m, 0x10000, 0x11003);
    assert_eq!(lookup(&mut c, &mut m, 0, 2), u32::MAX);
    assert_eq!(lookup(&mut c, &mut m, 0x10000, 2), 0x11000);
    c.x[1] = 0; // invalidating colliding VPN also evicts the indexed entry
    execute(&mut c, &mut m, word(2, 0, 1, 0));
    assert_eq!(lookup(&mut c, &mut m, 0x10000, 2), u32::MAX);
    assert_eq!(lookup(&mut c, &mut m, 0x1000, 2), 0x10000);
    execute(&mut c, &mut m, 0x0000_100f); // FENCE.I is not a TLB flush
    assert_eq!(lookup(&mut c, &mut m, 0x1000, 2), 0x10000);
    c.x[3] = c.pc + 16;
    execute(&mut c, &mut m, 0x0001_c07b); // XENTER x3
    assert_eq!(lookup(&mut c, &mut m, 0x1000, 2), 0x10000);
    // Simulated first publication of code watching: flush ALL alias write
    // proofs, then software may refill read/fetch only for the watched PA.
    execute(&mut c, &mut m, word(3, 0, 0, 0));
    for address in [0x1000, 0x2000] {
        assert_eq!(lookup(&mut c, &mut m, address, 6), u32::MAX);
        fill(&mut c, &mut m, address, 0x10005);
        assert_eq!(lookup(&mut c, &mut m, address, 2), 0x10000);
        assert_eq!(lookup(&mut c, &mut m, address, 10), 0x10000);
        assert_eq!(lookup(&mut c, &mut m, address, 14), u32::MAX);
    }
    // Leave returns to a fresh address; it must preserve table/window.
    execute(&mut c, &mut m, 0x0000_507b);
    assert_eq!(lookup(&mut c, &mut m, 0x2000, 2), 0x10000);
}

#[test]
fn guest_tlb_illegal_words_and_region_changes_are_precise() {
    let invalid = [
        (word(0, 3, 1, 3), 0x10000, 0x18000),  // reserved width
        (word(0, 3, 1, 16), 0x10000, 0x18000), // reserved mode
        (word(1, 3, 1, 2), 0x10000, 0x18000),  // nonzero rd
        (word(1, 0, 1, 66), 0x10000, 0x18000), // reserved funct7
        (word(1, 0, 1, 2), 0x10000, 0x10004),  // X in data fill
        (word(1, 0, 1, 34), 0x10000, 0x10001), // R in fetch fill
        (word(1, 0, 1, 34), 0x10000, 0x10002), // W in fetch fill
        (word(1, 0, 1, 34), 0x10000, 0x10008), // fetch reserved metadata
        (word(1, 0, 1, 2), 0x10000, 0x10008),  // metadata reserved bits
        (word(2, 0, 1, 1), 0x10000, 0x18000),
        (word(3, 0, 1, 0), 0x10000, 0x18000),
        (word(4, 3, 1, 2), 0x10000, 0x18000),
        (word(4, 0, 1, 34), 0x10000, 0x18000),
        (word(4, 0, 1, 2), 0x10001, 0x18000), // unaligned base
        (word(4, 0, 1, 2), 0x10000, 0x18001), // unaligned limit
        (word(4, 0, 1, 2), 0xffff_f000, 0),   // wrapped region
    ];
    for (bad, a, b) in invalid {
        let mut c = Core::new(0x1000);
        let mut m = Memory::new(0x20000);
        region(&mut c, &mut m, 0x10000, 0x18000);
        fill(&mut c, &mut m, 0x2000, 0x10007);
        c.x[1] = a;
        c.x[2] = b;
        c.flags = 1;
        c.pending_flags = 0x8d4;
        let before = (
            c.x,
            c.pc,
            c.guest_dtlb,
            c.guest_itlb,
            c.guest_region,
            c.flags,
            c.pending_flags,
        );
        m.write(c.pc, &bad.to_le_bytes()).unwrap();
        let memory_before = m.ram.clone();
        assert_eq!(
            c.step(&mut m).unwrap(),
            Some(Trap {
                pc: c.pc,
                cause: Cause::IllegalInstruction,
                value: bad
            })
        );
        assert_eq!(
            (
                c.x,
                c.pc,
                c.guest_dtlb,
                c.guest_itlb,
                c.guest_region,
                c.flags,
                c.pending_flags
            ),
            before
        );
        assert_eq!(m.ram, memory_before);
    }
}

/// Host-side policy test only: reuse the real x86 walker rather than inventing
/// a second interpretation of PTEs. The little guest bus owns a DISJOINT slice
/// of native RAM. This does not count the walk as simulated RV instructions.
struct GuestRam<'a>(&'a mut Memory);
const GUEST_BASE: u32 = 0x10000;
const GUEST_LEN: u64 = 0x8000;
impl r86::x86::Bus for GuestRam<'_> {
    fn read_phys_u8(&mut self, addr: r86::x86::PhysAddr) -> Result<u8, r86::x86::MemError> {
        self.probe_phys_write_u8(addr)?;
        Ok(self.0.ram[GUEST_BASE as usize + addr.0 as usize])
    }
    fn probe_phys_write_u8(&mut self, addr: r86::x86::PhysAddr) -> Result<(), r86::x86::MemError> {
        if addr.0 < GUEST_LEN {
            Ok(())
        } else {
            Err(r86::x86::MemError::OutOfRange { addr })
        }
    }
    fn write_phys_u8(
        &mut self,
        addr: r86::x86::PhysAddr,
        value: u8,
    ) -> Result<(), r86::x86::MemError> {
        self.probe_phys_write_u8(addr)?;
        self.0.ram[GUEST_BASE as usize + addr.0 as usize] = value;
        Ok(())
    }
}

fn walk_ram(
    cpu: &mut r86::x86::Cpu,
    memory: &mut Memory,
    linear: u32,
    write: bool,
    cpl: u8,
) -> Result<Option<u32>, r86::x86::mmu::Fault> {
    use r86::x86::{AccessKind, LinearAddr, mmu};
    let access = if write {
        AccessKind::Write
    } else {
        AccessKind::Read
    };
    let translation =
        mmu::translate_detailed(cpu, &mut GuestRam(memory), LinearAddr(linear), access, cpl)?;
    // Page-table presence is not proof of native backing! Guest ROM/MMIO/gaps
    // go back to the bus implementation, without installing a direct pointer.
    Ok((translation.phys.0 < GUEST_LEN).then(|| GUEST_BASE + translation.phys.0 as u32))
}

#[test]
fn guest_tlb_software_walk_composes_spaces_permissions_and_ad_effects() {
    use r86::x86::{
        Cpu,
        cpu::{CR0_PG, CR0_WP},
        mmu::Fault,
    };
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    let mut cpu = Cpu::new();
    cpu.enter_protected_flat(0, 0);
    cpu.cr0 |= CR0_PG | CR0_WP;
    cpu.cr3 = 0x1000; // guest physical, never native firmware address 0x1000
    region(&mut c, &mut m, GUEST_BASE, GUEST_BASE + GUEST_LEN as u32);
    m.write(GUEST_BASE + 0x1000, &0x2007u32.to_le_bytes())
        .unwrap();
    m.write(GUEST_BASE + 0x2010, &0x4007u32.to_le_bytes())
        .unwrap();
    m.write(0x4000, &0xfeed_faceu32.to_le_bytes()).unwrap(); // firmware canary
    m.write(GUEST_BASE + 0x4000, &0x1234_5678u32.to_le_bytes())
        .unwrap();

    let pointer = walk_ram(&mut cpu, &mut m, 0x4000, false, 3)
        .unwrap()
        .unwrap();
    assert_eq!(pointer, 0x14000);
    assert_eq!(m.read(0x11000, 32) & 0x20, 0x20);
    assert_eq!(m.read(0x12010, 32) & 0x60, 0x20); // read did not set D
    fill(&mut c, &mut m, 0x4000, pointer | 1);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 2), pointer);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 6), u32::MAX);
    assert_eq!(
        walk_ram(&mut cpu, &mut m, 0x4000, true, 3).unwrap(),
        Some(pointer)
    );
    assert_eq!(m.read(0x12010, 32) & 0x60, 0x60);
    fill(&mut c, &mut m, 0x4000, pointer | 3);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 14), pointer);
    c.x[2] = 0xabcd_1234;
    execute(&mut c, &mut m, 0x0020_a023); // SW x2,0(x1), with the returned native pointer
    assert_eq!(m.read(0x14000, 32), 0xabcd_1234);
    assert_eq!(m.read(0x4000, 32), 0xfeed_face);

    // A guest remap to PA zero still addresses guest RAM, not firmware zero.
    execute(&mut c, &mut m, word(3, 0, 0, 0));
    m.write(0x12010, &7u32.to_le_bytes()).unwrap();
    assert_eq!(
        walk_ram(&mut cpu, &mut m, 0x4000, false, 3).unwrap(),
        Some(GUEST_BASE)
    );
    // A guest PTE numerically naming our native base must not become a native
    // pointer: it names an unbacked guest PA, which the software bus must handle.
    m.write(0x12010, &(GUEST_BASE | 7).to_le_bytes()).unwrap();
    assert_eq!(walk_ram(&mut cpu, &mut m, 0x4000, false, 3).unwrap(), None);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 2), u32::MAX);

    // Guest permission changes require flushing old proofs even with the same
    // linear and physical addresses. A hardware service result is NOT the #PF.
    m.write(0x12010, &0x4005u32.to_le_bytes()).unwrap(); // user, read-only
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 6), u32::MAX);
    match walk_ram(&mut cpu, &mut m, 0x4000, true, 3) {
        Err(Fault::Page(fault)) => {
            assert_eq!(fault.code, 7);
            assert_eq!(fault.linear.0, 0x4000);
        }
        other => panic!("expected shared walker protection fault: {other:?}"),
    }
    assert_eq!(cpu.cr2, 0x4000);
    cpu.cr0 &= !CR0_WP;
    assert_eq!(
        walk_ram(&mut cpu, &mut m, 0x4000, true, 0).unwrap(),
        Some(0x14000)
    );
    fill(&mut c, &mut m, 0x4000, 0x14003);
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 6), 0x14000);
    execute(&mut c, &mut m, word(3, 0, 0, 0)); // before kernel -> user access
    assert!(matches!(
        walk_ram(&mut cpu, &mut m, 0x4000, true, 3),
        Err(Fault::Page(_))
    ));
    assert_eq!(lookup(&mut c, &mut m, 0x4000, 6), u32::MAX);
}
