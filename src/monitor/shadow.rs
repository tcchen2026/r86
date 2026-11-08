//! Resident address spaces and their physical page-table dependencies.
//! Translations come exclusively from the shared x86 walker.
use crate::x86::cpu::{CR0_PG, CR0_WP, CR4_PAE, CR4_PGE, CR4_PSE};
use crate::x86::{
    LinearAddr, PageTableFrameRole, PhysAddr, PhysFrame, PhysWrite, Translation,
    TranslationPageSize, WriteOrigin,
};

pub const RESIDENT_CONTEXTS: usize = 8;
const ALL_CONTEXTS: u8 = u8::MAX >> (u8::BITS as usize - RESIDENT_CONTEXTS);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowWindow {
    Supervisor,
    User,
}

/// One ARM L2 parent's x86 paging sources. Each 2 MiB linear span shares its
/// upper entries and contiguous leaf words, even when a guest 4 MiB leaf is
/// cached as individual 4 KiB pages. These are physical entry addresses, not
/// pointers or saved permission values; the shared walker alone admits maps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TableSource {
    pub upper: [u64; 2],
    pub leaves: u64,
    pub entry_bytes: u8,
    pub page_shift: u8,
}
impl TableSource {
    pub const EMPTY: Self = Self {
        upper: [u64::MAX; 2],
        leaves: u64::MAX,
        entry_bytes: 0,
        page_shift: 0,
    };

    pub fn from_walk(linear: LinearAddr, cr3: u32, cr4: u32, translation: Translation) -> Self {
        let pae = cr4 & CR4_PAE != 0;
        let width = if pae { 8 } else { 4 };
        let mut source = Self {
            upper: [
                if pae {
                    u64::from(cr3 & !31) + u64::from(linear.0 >> 30) * 8
                } else {
                    u64::MAX
                },
                u64::MAX,
            ],
            leaves: u64::MAX,
            entry_bytes: width,
            page_shift: match translation.page_size {
                TranslationPageSize::Size4K => 12,
                TranslationPageSize::Size2M => 21,
                TranslationPageSize::Size4M => 22,
                TranslationPageSize::Direct => unreachable!("unpaged shadow source"),
            },
        };
        for i in 0..translation.page_table_frame_count() as usize {
            let frame = translation.page_table_frame(i).unwrap();
            match frame.role {
                PageTableFrameRole::Directory => {
                    let index = if pae {
                        (linear.0 >> 21) & 511
                    } else {
                        linear.0 >> 22
                    };
                    source.upper[1] = frame.frame.base().0 + u64::from(index) * u64::from(width);
                }
                PageTableFrameRole::Table if source.page_shift == 12 => {
                    source.leaves = frame.frame.base().0
                        + if pae {
                            0
                        } else {
                            u64::from((linear.0 >> 21) & 1) * 2048
                        };
                }
                _ => {}
            }
        }
        source
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextKey {
    pub cr3: u32,
    pub cr0: u32,
    pub cr4: u32,
}
impl ContextKey {
    pub const fn new(cr0: u32, cr3: u32, cr4: u32) -> Self {
        Self {
            cr3,
            cr0: cr0 & (CR0_PG | CR0_WP),
            cr4: cr4 & (CR4_PAE | CR4_PSE | CR4_PGE),
        }
    }
}

#[derive(Clone, Copy)]
struct AddressSpace {
    key: Option<ContextKey>,
    last_used: u64,
    dirty: bool,
}
impl AddressSpace {
    const EMPTY: Self = Self {
        key: None,
        last_used: 0,
        dirty: false,
    };
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Activation {
    pub switched: bool,
    pub hit: bool,
    pub evicted: bool,
}

// Keep the dependency bytes 16-byte aligned so clearing one context's bits
// uses aligned vector loads/stores on the bare-metal AArch64 target.
#[repr(C, align(16))]
pub struct ContextCache<const FRAMES: usize> {
    spaces: [AddressSpace; RESIDENT_CONTEXTS],
    active: usize,
    clock: u64,
    // Bits survive a write until the dependency refresh completes. The legacy
    // standalone bus also uses them for its conservative software-cache flush.
    frame_users: [u8; FRAMES],
    // A context usually touches only a small part of guest RAM as page tables.
    // One bit per chunk limits clearing to those parts without changing the
    // per-frame dependency lookup used by tracked writes.
    used_chunks: [u64; RESIDENT_CONTEXTS],
}
impl<const FRAMES: usize> ContextCache<FRAMES> {
    const FRAMES_PER_CHUNK: usize = if FRAMES == 0 { 1 } else { FRAMES.div_ceil(64) };

    pub const fn new() -> Self {
        Self {
            spaces: [AddressSpace::EMPTY; RESIDENT_CONTEXTS],
            active: 0,
            clock: 0,
            frame_users: [0; FRAMES],
            used_chunks: [0; RESIDENT_CONTEXTS],
        }
    }
    pub fn active(&self) -> usize {
        self.active
    }
    pub fn key(&self) -> Option<ContextKey> {
        self.spaces[self.active].key
    }
    pub fn dirty(&self) -> bool {
        self.spaces[self.active].dirty
    }
    pub fn select(&mut self, key: ContextKey) -> Activation {
        if self.key() == Some(key) {
            return Activation::default();
        }
        self.clock += 1;
        let found = self.spaces.iter().position(|space| space.key == Some(key));
        let index = found.unwrap_or_else(|| {
            self.spaces
                .iter()
                .enumerate()
                .min_by_key(|(_, space)| space.last_used)
                .unwrap()
                .0
        });
        let evicted = found.is_none() && self.spaces[index].key.is_some();
        self.active = index;
        self.spaces[index].key = Some(key);
        self.spaces[index].last_used = self.clock;
        self.spaces[index].dirty |= evicted;
        Activation {
            switched: true,
            hit: found.is_some(),
            evicted,
        }
    }
    /// Retire recorded walk dependencies after completing required TLBI.
    /// A partial flush must restore surviving hardware-source ownership before
    /// another guest access can use the frame guards. Software TLB entries may
    /// survive independently; they do not require physical table-write guards.
    pub fn finish_flush(&mut self) {
        let keep = !(1 << self.active);
        let mut chunks = core::mem::take(&mut self.used_chunks[self.active]);
        while chunks != 0 {
            let first = chunks.trailing_zeros() as usize * Self::FRAMES_PER_CHUNK;
            let end = (first + Self::FRAMES_PER_CHUNK).min(FRAMES);
            for users in &mut self.frame_users[first..end] {
                *users &= keep;
            }
            chunks &= chunks - 1;
        }
        self.spaces[self.active].dirty = false;
    }
    pub fn invalidate_active(&mut self) {
        self.spaces[self.active].dirty = true;
    }
    /// Returns whether this context first learned that the frame is a page
    /// table, so the interpreter can revoke previously cached writable aliases.
    pub fn track_frame(&mut self, frame: PhysFrame) -> bool {
        let Some(users) = self.frame_users.get_mut(frame.0 as usize) else {
            return false;
        };
        let bit = 1 << self.active;
        let added = *users & bit == 0;
        *users |= bit;
        self.used_chunks[self.active] |= 1 << (frame.0 as usize / Self::FRAMES_PER_CHUNK);
        added
    }
    pub fn frame_has_users(&self, frame: PhysFrame) -> bool {
        self.frame_users
            .get(frame.0 as usize)
            .is_none_or(|users| *users != 0)
    }
    /// Track successful walks, including soft-only large-page leaves. Untracked
    /// physical page tables cannot supply native cache entries.
    pub fn track(&mut self, translation: Translation) -> bool {
        let mut tracked = true;
        for index in 0..translation.page_table_frame_count() as usize {
            let frame = translation.page_table_frame(index).unwrap().frame;
            if let Some(users) = self.frame_users.get_mut(frame.0 as usize) {
                *users |= 1 << self.active;
                self.used_chunks[self.active] |= 1 << (frame.0 as usize / Self::FRAMES_PER_CHUNK);
            } else {
                tracked = false;
            }
        }
        tracked
    }
    pub fn write_users(&self, write: PhysWrite) -> u8 {
        if write.len == 0 || write.origin == WriteOrigin::PageWalk {
            return 0;
        }
        let Some(end) = write.addr.0.checked_add(u64::from(write.len) - 1) else {
            return ALL_CONTEXTS;
        };
        let mut users = 0;
        for frame in write.first_frame().0..=PhysFrame::containing(PhysAddr(end)).0 {
            users |= self
                .frame_users
                .get(frame as usize)
                .copied()
                // track() refuses any mapping whose table dependencies do not
                // fit this array. Such a frame cannot own a resident mapping.
                // In particular, high PCI/MMIO stores are not page-table edits.
                .unwrap_or(0);
        }
        users
    }
    pub fn observe_write(&mut self, write: PhysWrite) -> u8 {
        let users = self.write_users(write);
        for (index, space) in self.spaces.iter_mut().enumerate() {
            if users & (1 << index) != 0 {
                space.dirty = true;
            }
        }
        users
    }
    pub fn write_affects_active(&self, write: PhysWrite) -> bool {
        self.write_users(write) & (1 << self.active) != 0
    }
}
impl<const FRAMES: usize> Default for ContextCache<FRAMES> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x86::mmu::WalkFrameSet;
    use crate::x86::{AccessKind, LinearAddr, PageTableFrameRole, TranslationPageSize};
    fn key(cr3: u32) -> ContextKey {
        ContextKey::new(CR0_PG | CR0_WP, cr3, 0)
    }
    fn translation(frame: u64) -> Translation {
        let mut frames = WalkFrameSet::new();
        frames.push(PhysFrame(frame), PageTableFrameRole::Table);
        Translation::new(
            PhysAddr(0x8000),
            TranslationPageSize::Size4K,
            frames,
            true,
            false,
            false,
            false,
        )
    }
    fn write(frame: u64) -> PhysWrite {
        PhysWrite::new(WriteOrigin::Cpu, PhysAddr(frame << 12), 4)
    }
    #[test]
    fn retains_clean_spaces_across_switches() {
        let mut cache = ContextCache::<16>::new();
        cache.select(key(0x1000));
        let a = cache.active();
        cache.track(translation(2));
        cache.select(key(0x3000));
        cache.track(translation(4));
        assert!(cache.select(key(0x1000)).hit);
        assert_eq!(cache.active(), a);
        assert!(!cache.dirty());
        assert!(cache.write_affects_active(write(2)));
    }
    #[test]
    fn inactive_and_shared_tables_are_invalidated() {
        let mut cache = ContextCache::<16>::new();
        cache.select(key(0x1000));
        cache.track(translation(2));
        cache.select(key(0x3000));
        cache.track(translation(4));
        cache.observe_write(write(2));
        assert!(!cache.dirty());
        assert!(!cache.write_affects_active(write(2)));
        cache.select(key(0x1000));
        assert!(cache.dirty());
        assert!(cache.write_affects_active(write(2)));
        cache.finish_flush();
        assert!(!cache.write_affects_active(write(2)));
        cache.track(translation(4));
        cache.observe_write(write(4));
        assert!(cache.dirty());
        cache.select(key(0x3000));
        assert!(cache.dirty());
    }
    #[test]
    fn shared_table_rewrites_reach_every_resident_space() {
        let mut cache = ContextCache::<16>::new();
        for slot in 0..RESIDENT_CONTEXTS {
            cache.select(key(slot as u32 * 0x1000));
            cache.track(translation(3));
        }
        assert_eq!(cache.observe_write(write(3)), ALL_CONTEXTS);
        for slot in 0..RESIDENT_CONTEXTS {
            assert!(cache.select(key(slot as u32 * 0x1000)).hit);
            assert!(cache.dirty());
            cache.finish_flush();
            assert!(!cache.write_affects_active(write(3)));
        }
        assert!(!cache.frame_has_users(PhysFrame(3)));
        // Out-of-capacity tables cannot supply a resident mapping. A write to
        // an unrelated high physical address must not discard all address spaces.
        assert!(!cache.track(translation(16)));
        assert_eq!(cache.observe_write(write(16)), 0);
        for slot in 0..RESIDENT_CONTEXTS {
            cache.select(key(slot as u32 * 0x1000));
            assert!(!cache.dirty());
        }
    }
    #[test]
    fn recycled_slot_forgets_old_dependencies_after_flush() {
        let mut cache = ContextCache::<16>::new();
        for n in 0..RESIDENT_CONTEXTS {
            cache.select(key(n as u32 * 0x1000));
            cache.track(translation(n as u64));
        }
        let selected = cache.select(key(0x8000));
        assert!(selected.evicted && cache.dirty());
        cache.finish_flush();
        cache.track(translation(8));
        cache.observe_write(write(0));
        assert!(!cache.dirty());
        cache.observe_write(write(8));
        assert!(cache.dirty());
    }
    #[test]
    fn accessed_dirty_writebacks_do_not_invalidate() {
        let mut cache = ContextCache::<16>::new();
        cache.select(key(0x1000));
        cache.track(translation(2));
        let mut ad = write(2);
        ad.origin = WriteOrigin::PageWalk;
        assert_eq!(cache.observe_write(ad), 0);
        assert!(!cache.dirty());
        for origin in [
            WriteOrigin::Cpu,
            WriteOrigin::Device,
            WriteOrigin::Loader,
            WriteOrigin::Debug,
        ] {
            let mut write = write(2);
            write.origin = origin;
            assert_ne!(cache.observe_write(write), 0);
        }
    }
    #[test]
    fn split_writes_invalidate_only_admitted_dependencies() {
        let mut cache = ContextCache::<16>::new();
        cache.select(key(0x1000));
        cache.track(translation(2));
        assert!(cache.write_affects_active(PhysWrite::new(WriteOrigin::Cpu, PhysAddr(0x1fff), 2)));
        assert!(!cache.track(translation(16)));
        assert!(!cache.write_affects_active(write(16)));
        assert!(cache.write_affects_active(PhysWrite::new(
            WriteOrigin::Device,
            PhysAddr(u64::MAX),
            2
        )));
        assert!(cache.track(Translation::direct(LinearAddr(0), AccessKind::Read)));
    }

    #[test]
    fn flush_removes_all_active_dependencies_and_preserves_other_spaces() {
        // An odd frame count covers a partial final chunk, while frames in the
        // same and distant chunks exercise shared and independent dependencies.
        let mut cache = ContextCache::<131>::new();
        cache.select(key(0x1000));
        let a = cache.active();
        for frame in [0, 1, 64, 130] {
            assert!(cache.track_frame(PhysFrame(frame)));
        }
        cache.select(key(0x2000));
        let b = cache.active();
        for frame in [1, 65, 130] {
            assert!(cache.track(translation(frame)));
        }
        cache.select(key(0x1000));
        cache.finish_flush();
        for frame in 0..131 {
            assert_eq!(cache.write_users(write(frame)) & (1 << a), 0);
            assert_eq!(
                cache.write_users(write(frame)) & (1 << b) != 0,
                [1, 65, 130].contains(&frame)
            );
        }
        // Reusing the flushed context must register its next dependencies.
        assert!(cache.track(translation(129)));
        assert!(cache.write_affects_active(write(129)));
        cache.finish_flush();
        assert!(!cache.write_affects_active(write(129)));
        cache.select(key(0x2000));
        cache.finish_flush();
        for frame in 0..131 {
            assert_eq!(cache.write_users(write(frame)), 0);
        }

        let mut cache = ContextCache::<128>::new();
        cache.select(key(0x1000));
        assert!(cache.track_frame(PhysFrame(127)));
        assert!(cache.write_affects_active(write(127)));
        cache.finish_flush();
        assert!(!cache.write_affects_active(write(127)));
    }
}
