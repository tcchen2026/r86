//! Fixed block index for the Thumb-2 runtime: sets of [`WAYS`] records, no
//! heap growth and no rehashing. A key is (EIP, flat-segment facts, backing
//! byte of the execute translation). Inserting into a full set replaces one
//! record; its native code stays in the arena, reachable only through
//! dispatch-table entries that the runtime retires by its own rules.

use alloc::boxed::Box;

pub const WAYS: usize = 4;

#[derive(Clone, Copy)]
pub struct Record {
    pub eip: u32,
    pub facts: u32,
    pub backing: u64,
    pub version: u64,
    /// CodePages version cell of the block's page; null marks an empty record.
    pub version_ptr: *const u64,
    /// Native entry with the Thumb bit set, or 0 when the first instruction
    /// needs the interpreter.
    pub address: u32,
    pub instructions: u32,
    /// Guest bytes a native block depends on, kept in the code arena.
    pub source: *const u8,
    pub source_len: u32,
    #[cfg(feature = "jit-profile")]
    pub opcode: u16,
}

impl Record {
    const EMPTY: Self = Self {
        eip: 0,
        facts: 0,
        backing: 0,
        version: 0,
        version_ptr: core::ptr::null(),
        address: 0,
        instructions: 0,
        source: core::ptr::null(),
        source_len: 0,
        #[cfg(feature = "jit-profile")]
        opcode: 0,
    };

    fn is(&self, eip: u32, facts: u32, backing: u64) -> bool {
        !self.version_ptr.is_null()
            && self.eip == eip
            && self.facts == facts
            && self.backing == backing
    }
}

pub struct Store {
    records: Box<[Record]>,
    /// Number of sets minus one.
    mask: usize,
    /// Next way to replace in a full set.
    rotor: usize,
    live: usize,
}

impl Store {
    /// `blocks` records in `blocks / WAYS` sets; a power of two.
    pub fn new(blocks: usize) -> Self {
        assert!(blocks.is_power_of_two() && blocks >= WAYS);
        Self {
            records: alloc::vec![Record::EMPTY; blocks].into_boxed_slice(),
            mask: blocks / WAYS - 1,
            rotor: 0,
            live: 0,
        }
    }

    fn set(&self, eip: u32, backing: u64) -> usize {
        ((eip ^ (eip >> 12) ^ (backing as u32 >> 7)) as usize & self.mask) * WAYS
    }

    /// Slot of the record with this key.
    pub fn find(&self, eip: u32, facts: u32, backing: u64) -> Option<usize> {
        let set = self.set(eip, backing);
        (set..set + WAYS).find(|&slot| self.records[slot].is(eip, facts, backing))
    }

    pub fn get(&self, slot: usize) -> Record {
        self.records[slot]
    }

    /// Record a new version of an unchanged block.
    pub fn revalidate(&mut self, slot: usize, version: u64, version_ptr: *const u64) {
        self.records[slot].version = version;
        self.records[slot].version_ptr = version_ptr;
    }

    pub fn remove(&mut self, slot: usize) {
        if !self.records[slot].version_ptr.is_null() {
            self.records[slot] = Record::EMPTY;
            self.live -= 1;
        }
    }

    /// Insert `record`, which must have a version cell. An equal key is
    /// replaced, then an empty way is used, then a live record is replaced in
    /// rotation. Returns true when a record with another key was lost.
    pub fn insert(&mut self, record: Record) -> bool {
        assert!(!record.version_ptr.is_null());
        let set = self.set(record.eip, record.backing);
        if let Some(slot) = self.find(record.eip, record.facts, record.backing) {
            self.records[slot] = record;
            return false;
        }
        if let Some(slot) = (set..set + WAYS).find(|&slot| self.records[slot].version_ptr.is_null())
        {
            self.records[slot] = record;
            self.live += 1;
            return false;
        }
        self.rotor = (self.rotor + 1) % WAYS;
        self.records[set + self.rotor] = record;
        true
    }

    pub fn clear(&mut self) {
        self.records.fill(Record::EMPTY);
        self.live = 0;
    }

    /// Number of live records.
    pub fn live(&self) -> usize {
        self.live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static CELL: u64 = 7;

    fn record(eip: u32, facts: u32, backing: u64) -> Record {
        Record {
            eip,
            facts,
            backing,
            version: 7,
            version_ptr: &CELL,
            address: eip | 1,
            ..Record::EMPTY
        }
    }

    #[test]
    fn keys_are_exact_and_removal_is_per_record() {
        let mut store = Store::new(64);
        assert_eq!(store.live(), 0);
        assert!(!store.insert(record(0x1000, 1, 0x4000)));
        assert!(!store.insert(record(0x1000, 2, 0x4000)));
        assert!(!store.insert(record(0x1000, 1, 0x8000)));
        assert_eq!(store.live(), 3);
        assert!(store.find(0x1000, 3, 0x4000).is_none());
        assert!(store.find(0x1004, 1, 0x4000).is_none());
        let slot = store.find(0x1000, 2, 0x4000).unwrap();
        assert_eq!(store.get(slot).facts, 2);
        store.revalidate(slot, 9, &CELL);
        assert_eq!(store.get(slot).version, 9);
        store.remove(slot);
        store.remove(slot);
        assert_eq!(store.live(), 2);
        assert!(store.find(0x1000, 2, 0x4000).is_none());
        assert!(store.find(0x1000, 1, 0x4000).is_some());
        // Reinserting a key replaces it in place.
        let mut again = record(0x1000, 1, 0x4000);
        again.instructions = 5;
        assert!(!store.insert(again));
        assert_eq!(store.live(), 2);
        let slot = store.find(0x1000, 1, 0x4000).unwrap();
        assert_eq!(store.get(slot).instructions, 5);
        store.clear();
        assert_eq!(store.live(), 0);
        assert!(store.find(0x1000, 1, 0x4000).is_none());
    }

    #[test]
    fn a_full_set_replaces_one_record_and_keeps_the_others() {
        let mut store = Store::new(64);
        // Facts are not hashed, so these keys share one set.
        for facts in 0..WAYS as u32 {
            assert!(!store.insert(record(0x2000, facts, 0x4000)));
        }
        assert!(store.insert(record(0x2000, 100, 0x4000)));
        assert_eq!(store.live(), WAYS);
        assert!(store.find(0x2000, 100, 0x4000).is_some());
        let kept = (0..WAYS as u32)
            .filter(|&facts| store.find(0x2000, facts, 0x4000).is_some())
            .count();
        assert_eq!(kept, WAYS - 1);
        // Successive replacements rotate through the set.
        assert!(store.insert(record(0x2000, 101, 0x4000)));
        assert!(store.find(0x2000, 100, 0x4000).is_some());
        assert!(store.find(0x2000, 101, 0x4000).is_some());
    }
}
