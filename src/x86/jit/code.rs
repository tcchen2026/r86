//! AArch64 code publication, packing and bounded entry-table ownership.
//! Linux and EL1 select their mapping owner at build time. Both publish through
//! an RW/NX alias and execute through a distinct RO/RX alias, with no live writes.
//!
//! Publish and reclaim only outside native execution. Linux uses one private
//! memfd; EL1 uses dedicated monitor storage shared by all ASID roots. Neither
//! code alias is RWX. Every arena starts empty, and generated code is compiled
//! in the current execution and never restored from a file. Optional Linux
//! dumps are for offline analysis only, with no generated-code loading path.
//! This is not physical-page write/execute exclusion: the aliases share backing.
//! Exclusive, single-thread arena ownership prohibits writes while any generated
//! code is active. Guest RAM never contains either alias. No helper may compile
//! recursively while generated code from this arena is on the stack.
//! The fixed dense-entry pool follows the code area in both aliases. Native
//! offsets are signed relative to the table's RX address, avoiding another
//! code-base load at dispatch. Rust mutates tables only through arena-borrowed
//! RW slices. Clear retires all page/table indices before reusing either area;
//! the extra metadata mapping reserves address space, committing tables on use.
//!
//! The alias addresses can differ: clean D-cache by the writer address, invalidate
//! I-cache by the execution address, then publish an RX entry. This avoids two
//! mprotect calls for each of hundreds of thousands of small cold-boot blocks.
//! Consulted FEX SharedCodeBufferManager.cpp and JIT.cpp for buffer/publication
//! costs; this adapter's ownership, encodings and cache maintenance are local.

use super::frame::Frame;
use alloc::rc::Rc;
use core::marker::PhantomData;
use core::mem::size_of;
#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux::Mapping;
#[cfg(target_os = "none")]
pub(crate) mod monitor;
#[cfg(target_os = "none")]
use monitor::Mapping;

pub(super) const ENTRY_TABLES: usize = 4096;
type EntryWords = [u32; super::links::PAGE_BYTES];

/// Constructed only by the arena's bounded table allocator. The fixed pool is
/// present in every arena; slices still borrow their actual mapping owner.
#[derive(Clone, Copy)]
pub(super) struct TableIndex(u16);

#[derive(Clone, Copy, Debug)]
pub struct CodeHandle {
    start: usize,
    end: usize,
    epoch: u64,
}

impl CodeHandle {
    #[cfg(target_os = "linux")]
    pub(super) fn byte_len(self) -> usize {
        self.end - self.start
    }
}

#[derive(Debug)]
pub enum CodeError {
    #[cfg(target_os = "linux")]
    Os(io::Error),
    #[cfg(target_os = "none")]
    MonitorUnavailable,
    Capacity,
    Empty,
    StaleHandle,
}

pub struct CodeArena {
    mapping: Mapping,
    capacity: usize,
    entries_start: usize,
    entries_used: usize,
    used: usize,
    hot_cursor: usize,
    cold_cursor: usize,
    epoch: u64,
    _thread_owner: PhantomData<Rc<()>>,
}

impl CodeArena {
    pub fn new(capacity: usize) -> Result<Self, CodeError> {
        let (mapping, entries_start) =
            Mapping::new(capacity, ENTRY_TABLES * size_of::<EntryWords>())?;
        Ok(Self {
            mapping,
            capacity,
            entries_start,
            entries_used: 0,
            used: 0,
            hot_cursor: 0,
            cold_cursor: 0,
            epoch: 1,
            _thread_owner: PhantomData,
        })
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

    pub(super) fn allocate_entries(&mut self) -> TableIndex {
        assert!(self.entries_used < ENTRY_TABLES, "JIT entry pool exhausted");
        let index = TableIndex(self.entries_used as u16);
        self.entries_used += 1;
        self.entries_mut(index).fill(0);
        index
    }

    pub(super) fn entries(&self, index: TableIndex) -> &EntryWords {
        let offset = self.entries_start + usize::from(index.0) * size_of::<EntryWords>();
        // SAFETY: only this module constructs indices, all are below ENTRY_TABLES.
        // The complete aligned pool is initialized shared backing. The slice
        // borrows this live owner; publication/native execution are exclusive.
        unsafe {
            &*self
                .mapping
                .writable
                .as_ptr()
                .add(offset)
                .cast::<EntryWords>()
        }
    }

    pub(super) fn entries_mut(&mut self, index: TableIndex) -> &mut EntryWords {
        let offset = self.entries_start + usize::from(index.0) * size_of::<EntryWords>();
        // SAFETY: same bounds as entries(), with exclusive ownership. No native
        // invocation or other table borrow exists during a mutation.
        unsafe {
            &mut *self
                .mapping
                .writable
                .as_ptr()
                .add(offset)
                .cast::<EntryWords>()
        }
    }

    pub(super) fn entries_pointer(&self, index: TableIndex) -> *const u32 {
        let offset = self.entries_start + usize::from(index.0) * size_of::<EntryWords>();
        // SAFETY: the opaque bounded index names a complete table in the alias.
        unsafe { self.mapping.base.as_ptr().add(offset).cast() }
    }

    /// Explicit developer diagnostic, outside native execution. The raw image
    /// and absolute-address ranges let perf samples distinguish shared entry,
    /// dispatch and return work from guest blocks without adding native counters.
    /// This represents the current arena epoch; samples from reclaimed epochs
    /// must not be attributed to its replacement bytes.
    #[cfg(target_os = "linux")]
    pub(super) fn dump(&self, path: &std::path::Path, ranges: &[(usize, &str)]) -> io::Result<()> {
        use std::io::Write;
        // SAFETY: used bytes are initialized publications in this live mapping;
        // the exclusive owner never publishes or patches while this borrow runs.
        let bytes =
            unsafe { core::slice::from_raw_parts(self.mapping.writable.as_ptr(), self.used) };
        std::fs::write(path, bytes)?;
        let mut map = io::BufWriter::new(std::fs::File::create(path.with_extension("map"))?);
        for (index, &(start, name)) in ranges.iter().enumerate() {
            let end = ranges.get(index + 1).map_or(self.used, |next| next.0);
            assert!(start <= end && end <= self.used);
            if start != end {
                writeln!(
                    map,
                    "{:x} {:x} {name}",
                    self.mapping.base.as_ptr() as usize + start,
                    end - start
                )?;
            }
        }
        map.flush()
    }

    pub fn publish(&mut self, words: &[u32]) -> Result<CodeHandle, CodeError> {
        if words.is_empty() {
            return Err(CodeError::Empty);
        }
        let bytes = words.len().checked_mul(4).ok_or(CodeError::Capacity)?;
        let end = self.used.checked_add(bytes).ok_or(CodeError::Capacity)?;
        if end > self.capacity {
            return Err(CodeError::Capacity);
        }
        // SAFETY: publication exclusively owns the arena; the whole destination
        // lies in its RW alias. No native stack is active, u32/mmap are aligned,
        // and cache synchronization completes before an RX handle is returned.
        unsafe {
            let destination = self.mapping.writable.as_ptr().add(self.used);
            core::ptr::copy_nonoverlapping(words.as_ptr(), destination.cast(), words.len());
            synchronize_instructions(
                destination,
                self.mapping.base.as_ptr().add(self.used),
                &[(0, bytes)],
            );
        }
        let handle = CodeHandle {
            start: self.used,
            end,
            epoch: self.epoch,
        };
        self.used = end;
        Ok(handle)
    }

    /// Pack ordinary execution together and keep its exits at the other end of
    /// a 32 KiB region. Every TBZ remains in range, without extra hot branches.
    /// Only current-block fixups cross the split; fault sites and linked edges
    /// remain offsets in its contiguous hot body.
    pub fn publish_block(
        &mut self,
        code: &mut super::compile::CompiledBlock,
    ) -> Result<CodeHandle, CodeError> {
        let hot_len = code.cold_word * 4;
        let cold_len = (code.words.len() - code.cold_word) * 4;
        let bytes = hot_len + cold_len;
        assert!(hot_len != 0 && cold_len != 0 && bytes < 32 * 1024);
        let padding = self.hot_cursor.wrapping_neg() & 15;
        if self.cold_cursor - self.hot_cursor < bytes + padding {
            let available = self.capacity - self.used;
            let region = available.min(32 * 1024);
            let padding = self.used.wrapping_neg() & 15;
            if region < bytes + padding {
                return Err(CodeError::Capacity);
            }
            self.hot_cursor = self.used;
            self.cold_cursor = self.used + region;
            self.used += region;
        }
        self.hot_cursor = (self.hot_cursor + 15) & !15;
        let start = self.hot_cursor;
        self.cold_cursor -= cold_len;
        self.hot_cursor += hot_len;
        #[cfg(any(test, all(feature = "monitor", target_os = "none")))]
        for recovery in &mut code.recoveries {
            assert!(*recovery as usize >= hot_len);
            *recovery += (self.cold_cursor - self.hot_cursor) as u32;
        }
        for relocation in &code.relocations {
            relocation.split(
                &mut code.words,
                code.cold_word,
                self.cold_cursor - self.hot_cursor,
            );
        }
        for (offset, words) in [
            (start, &code.words[..code.cold_word]),
            (self.cold_cursor, &code.words[code.cold_word..]),
        ] {
            // SAFETY: the two cursors reserve disjoint, previously unpublished
            // ranges inside the mapping. Publication has exclusive ownership;
            // synchronize both regions together before allowing native entry.
            unsafe {
                let destination = self.mapping.writable.as_ptr().add(offset);
                core::ptr::copy_nonoverlapping(words.as_ptr(), destination.cast(), words.len());
            }
        }
        // SAFETY: both copied regions are initialized and exclusively owned.
        // One publication orders all writes before any RX handle becomes live.
        unsafe {
            synchronize_instructions(
                self.mapping.writable.as_ptr(),
                self.mapping.base.as_ptr(),
                &[(start, hot_len), (self.cold_cursor, cold_len)],
            );
        }
        Ok(CodeHandle {
            start,
            end: self.hot_cursor,
            epoch: self.epoch,
        })
    }

    pub fn clear(&mut self) {
        // Links and native stacks must already be gone. Epochs reject old
        // handles; subsequent publication replaces the bytes and synchronizes
        // both aliases before the same virtual addresses can be called again.
        self.used = 0;
        self.entries_used = 0;
        self.hot_cursor = 0;
        self.cold_cursor = 0;
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("JIT arena epoch exhausted");
    }

    /// Replace a recorded same-page edge, outside all native invocations. Both
    /// addresses must belong to current published code; the page owner proves
    /// that source and target share validity and discards both on invalidation.
    pub fn patch_branch(&mut self, source: usize, target: usize) -> bool {
        let offset = |address: usize| {
            address
                .checked_sub(self.mapping.base.as_ptr() as usize)
                .filter(|&offset| offset < self.used && offset & 3 == 0)
                .expect("JIT link outside current published arena")
        };
        let source_offset = offset(source);
        let target_offset = offset(target);
        // SAFETY: checked aligned publication bounds, exclusive arena ownership
        // and no active native code. Synchronize both aliases before re-entry.
        unsafe {
            let writer = self.mapping.writable.as_ptr().add(source_offset);
            let Some(word) = super::a64::retarget_branch(
                writer.cast::<u32>().read(),
                target_offset as isize - source_offset as isize,
            ) else {
                return false;
            };
            writer.cast::<u32>().write(word);
            synchronize_instructions(
                writer,
                self.mapping.base.as_ptr().add(source_offset),
                &[(0, 4)],
            );
        }
        true
    }

    /// # Safety
    /// The handle must originate from this arena. The published entry must obey
    /// the frame/AAPCS64 ABI, return normally, and
    /// access only valid guest/adapter data. Its helpers must not mutate this
    /// arena. Publication checks bytes/capacity, not the semantics of native code.
    pub unsafe fn invoke(&self, handle: CodeHandle, frame: &mut Frame) -> Result<(), CodeError> {
        #[cfg(target_os = "none")]
        let _invocation = super::hard::Invocation::enter(frame);
        let address = self.address(handle, 0)?;
        // SAFETY: checked handle lies inside published RX bytes. The caller
        // establishes that the entry has this exact ABI and code behavior.
        unsafe {
            let entry: unsafe extern "C" fn(*mut Frame) = core::mem::transmute(address);
            entry(frame);
        }
        Ok(())
    }

    /// Address of an instruction in a published block, valid only until the next
    /// reclamation. The link owner must clear its table before clearing the arena.
    pub fn address(&self, handle: CodeHandle, offset: usize) -> Result<usize, CodeError> {
        if handle.epoch != self.epoch || handle.end > self.used || handle.start >= handle.end {
            return Err(CodeError::StaleHandle);
        }
        if offset >= handle.end - handle.start || offset & 3 != 0 {
            return Err(CodeError::StaleHandle);
        }
        Ok(self.mapping.base.as_ptr() as usize + handle.start + offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_access_recovery_keeps_its_exact_cold_exit() {
        let decoded = super::super::decode::block(
            &[0x40, 0x8b, 0x03, 0x8b, 0x4b, 4, 0x01, 0x4b, 8, 0xf3, 0xa5],
            0x1000,
            32,
        );
        let mut code = super::super::compile::block(&decoded, None, 0).unwrap();
        assert!(code.faults.len() >= 4);
        let recovery_words: alloc::vec::Vec<_> = code
            .recoveries
            .iter()
            .map(|&offset| code.words[offset as usize / 4])
            .collect();
        let mut arena = CodeArena::new(64 * 1024).unwrap();
        let handle = arena.publish_block(&mut code).unwrap();
        let body = arena.address(handle, 0).unwrap();
        for ((fault, &recovery), word) in
            code.faults.iter().zip(&code.recoveries).zip(recovery_words)
        {
            // Each native access still names the same x86 instruction's cold
            // exit after packing hot blocks together and moving their tails.
            assert!(recovery > fault.native_offset);
            assert_eq!(
                unsafe { ((body + recovery as usize) as *const u32).read() },
                word
            );
            assert_eq!(
                unsafe { ((body + fault.native_offset as usize) as *const u32).read() },
                code.words[fault.native_offset as usize / 4]
            );
        }
    }

    #[test]
    fn entry_tables_publish_to_native_alias_and_reset_without_overlapping_code() {
        let mut arena = CodeArena::new(4096).unwrap();
        let code = arena.publish(&[0xd65f_03c0]).unwrap(); // RET
        let entry = arena.address(code, 0).unwrap();
        let first = arena.allocate_entries();
        let first_native = arena.entries_pointer(first);
        let displacement = i32::try_from(entry as isize - first_native as isize).unwrap();
        assert!(displacement < 0);
        arena.entries_mut(first)[17] = displacement as u32;
        let second = arena.allocate_entries();
        arena.entries_mut(second)[17] = super::super::links::INTERPRET;
        // SAFETY: both aliases are alive; table publication is complete and no
        // native invocation or Rust mutable borrow is active during this read.
        assert_eq!(unsafe { first_native.add(17).read() }, displacement as u32);
        assert_eq!(arena.entries(first)[17], displacement as u32);
        assert_eq!(arena.used_bytes(), 4);
        assert_eq!(arena.address(code, 0).unwrap(), entry);

        arena.clear();
        let fresh = arena.allocate_entries();
        assert_eq!(arena.entries_pointer(fresh), first_native);
        assert!(arena.entries(fresh).iter().all(|&word| word == 0));
        assert!(matches!(
            arena.address(code, 0),
            Err(CodeError::StaleHandle)
        ));
    }
}

/// Both adapters use the architectural D-cache clean / I-cache invalidate
/// sequence, like the existing KVM loader. Read the actual host cache-line sizes;
/// neither a 4 KiB guest page nor an assumed 64-byte line is a host ABI guarantee.
/// All regions have been written before this call. IDC/DIC can remove maintenance
/// operations, but publication still orders stores and synchronizes execution.
/// Consulted LLVM compiler-rt's clear_cache.c and Linux arm64/mm/cache.S for
/// those feature bits; RW and RX aliases require their distinct addresses here.
unsafe fn synchronize_instructions(
    writer: *mut u8,
    executable: *const u8,
    regions: &[(usize, usize)],
) {
    use core::arch::asm;
    let ctr: usize;
    // SAFETY: both aliases span the same live backing bytes. Barriers
    // include compiler memory effects; stores must precede code publication.
    unsafe {
        asm!("mrs {ctr}, ctr_el0", ctr = out(reg) ctr, options(nostack, preserves_flags));
        if ctr & (1 << 28) == 0 {
            let line = 4_usize << ((ctr >> 16) & 15);
            for &(offset, len) in regions {
                let start = writer as usize + offset;
                let end = start + len;
                let mut address = start & !(line - 1);
                while address < end {
                    asm!("dc cvau, {address}", address = in(reg) address, options(nostack, preserves_flags));
                    address += line;
                }
            }
        }
        asm!("dsb ish", options(nostack, preserves_flags));
        if ctr & (1 << 29) == 0 {
            let line = 4_usize << (ctr & 15);
            for &(offset, len) in regions {
                let start = executable as usize + offset;
                let end = start + len;
                let mut address = start & !(line - 1);
                while address < end {
                    asm!("ic ivau, {address}", address = in(reg) address, options(nostack, preserves_flags));
                    address += line;
                }
            }
            asm!("dsb ish", options(nostack, preserves_flags));
        }
        asm!("isb", options(nostack, preserves_flags));
    }
}
