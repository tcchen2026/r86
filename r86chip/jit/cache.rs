//! Compact admission keys and one stable code generation. Displacing a key does
//! not free its code: direct links remain valid until the whole generation is
//! retired. Resetting two lengths and an epoch replaces restoration scans.
use super::compile::{Block, ENTRY_OFFSET};
use alloc::{boxed::Box, vec, vec::Vec};

pub(super) const SETS: usize = 1024;
pub(super) const CODE_WORDS: usize = 8 * 1024 * 1024 / 4;
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Key {
    eip: u32,
    epoch: u32,
}
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct Keys([Key; 2]);
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub(super) struct Value {
    /// 0..2: admission heat, 3: negative entry, >=4: native entry address.
    pub pc: u32,
    pub backing: u32,
}
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct Values([Value; 2]);
#[derive(Clone, Copy)]
#[repr(C, align(8))]
pub(super) struct Refund {
    pub eip: u32,
    pub count: u32,
}

pub(super) struct Cache {
    keys: Box<[Keys; SETS]>,
    values: Box<[Values; SETS]>,
    pub epoch: u32,
    code: Vec<u32>,
    sites: Vec<Refund>,
    pub dirty: bool,
}
fn set(eip: u32) -> usize {
    ((eip ^ (eip >> 12) ^ (eip >> 4)) as usize) & (SETS - 1)
}
impl Cache {
    pub fn new() -> Self {
        Self {
            keys: vec![Keys::default(); SETS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            values: vec![Values::default(); SETS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            epoch: 2,
            code: Vec::with_capacity(CODE_WORDS),
            sites: Vec::with_capacity(CODE_WORDS),
            dirty: false,
        }
    }
    pub fn clear(&mut self) {
        self.epoch = self
            .epoch
            .checked_add(2)
            .expect("chip code generation exhausted");
        self.code.clear();
        self.sites.clear();
        self.dirty = true;
    }
    /// Both probes share one key line, and both payloads share one other line.
    /// Bit zero of way 0's epoch holds replacement state; generations are even.
    pub fn slot(&mut self, eip: u32) -> (usize, bool) {
        let at = set(eip);
        let keys = &mut self.keys[at].0;
        let live = keys.map(|k| k.epoch & !1 == self.epoch);
        let way = if live[0] && keys[0].eip == eip {
            false
        } else if live[1] && keys[1].eip == eip {
            true
        } else {
            let values = &mut self.values[at].0;
            let way = if !live[0] || values[0].pc < 4 {
                false
            } else if !live[1] || values[1].pc < 4 {
                true
            } else {
                keys[0].epoch ^= 1;
                keys[0].epoch & 1 != 0
            };
            let n = usize::from(way);
            keys[n] = Key {
                eip,
                epoch: self.epoch | if !way { keys[0].epoch & 1 } else { 0 },
            };
            values[n] = Value::default();
            way
        };
        (at, way)
    }
    pub fn value(&self, (at, way): (usize, bool)) -> Value {
        self.values[at].0[usize::from(way)]
    }
    pub fn put(&mut self, (at, way): (usize, bool), value: Value) {
        self.values[at].0[usize::from(way)] = value;
    }
    pub fn find(&self, eip: u32) -> Option<u32> {
        let at = set(eip);
        let keys = self.keys[at].0;
        let way = if keys[0].epoch & !1 == self.epoch && keys[0].eip == eip {
            false
        } else if keys[1].epoch & !1 == self.epoch && keys[1].eip == eip {
            true
        } else {
            return None;
        };
        let pc = self.values[at].0[usize::from(way)].pc;
        (pc >= 4).then_some(pc)
    }
    pub fn fits(&self, block: &Block) -> bool {
        self.code.len() + block.words.len() <= CODE_WORDS
    }
    pub fn install(&mut self, block: &Block) -> u32 {
        assert!(self.fits(block), "chip block exceeds code arena");
        let base = self.code.len();
        self.code.extend_from_slice(&block.words);
        let mut current = block.sites[0];
        let mut next = 1;
        for word in 0..block.words.len() {
            while next < block.sites.len() && block.sites[next].offset <= word as u32 * 4 {
                current = block.sites[next];
                next += 1;
            }
            self.sites.push(Refund {
                eip: current.eip,
                count: block.instructions - current.index,
            });
        }
        self.dirty = true;
        self.code.as_ptr() as u32 + base as u32 * 4 + ENTRY_OFFSET
    }
    pub fn site(&self, pc: u32) -> Refund {
        let word = pc.wrapping_sub(self.code.as_ptr() as u32) / 4;
        *self
            .sites
            .get(word as usize)
            .expect("chip service outside live code")
    }
    pub fn patch(&mut self, pc: u32, word: u32) {
        let at = pc.wrapping_sub(self.code.as_ptr() as u32) / 4;
        self.code[at as usize] = word;
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexed_keys_collide_without_reclaiming_code_and_reset_by_epoch() {
        assert_eq!(core::mem::size_of::<Keys>(), 16);
        assert_eq!(core::mem::size_of::<Values>(), 16);
        assert_eq!(core::mem::size_of::<Refund>(), 8);
        let mut c = Cache::new();
        let mut saved = Vec::new();
        for eip in [0x2000, 0x2444, 0x2888] {
            // INC; ADD [EBX],ECX; INC. Keep all old service sites addressable
            // after key displacement, and refund precisely at each word.
            let block =
                super::super::compile::block(&[0x40, 0x01, 0x0b, 0x42], eip, 0, 16).unwrap();
            let pc = c.install(&block);
            for word in 1..block.words.len() {
                let offset = word as u32 * 4;
                let site = block.site(offset);
                saved.push((
                    pc - ENTRY_OFFSET + offset,
                    site.eip,
                    block.instructions - site.index,
                ));
            }
            let slot = c.slot(eip);
            c.put(
                slot,
                Value {
                    pc,
                    backing: 0x10000000 + eip,
                },
            );
            assert_eq!(c.value(slot).backing, 0x10000000 + eip);
            assert_eq!(c.find(eip), Some(pc));
        }
        assert!(c.find(0x2000).is_none() || c.find(0x2444).is_none());
        for (pc, eip, count) in saved {
            let site = c.site(pc);
            assert_eq!((site.eip, site.count), (eip, count));
        }
        let base = c.code.as_ptr();
        c.patch(base as u32 + ENTRY_OFFSET, 0x13);
        assert_eq!(c.code[1], 0x13);
        let block = super::super::compile::block(&[0x90], 0x8000, 0, 1).unwrap();
        c.code.resize(CODE_WORDS, 0);
        assert!(!c.fits(&block));
        c.clear();
        for eip in [0x2000, 0x2444, 0x2888] {
            assert_eq!(c.find(eip), None);
        }
        assert!(c.fits(&block));
        assert_eq!(c.install(&block), base as u32 + ENTRY_OFFSET);
        assert_eq!(c.site(base as u32 + ENTRY_OFFSET).eip, 0x8000);
    }
}
