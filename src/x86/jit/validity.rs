//! Physical backing-page content versions, shared by memory adapters.
//!
//! Code identity includes the actual backing page/offset, not CR3 or linear PC
//! alone. A block may execute while its execute translation and recorded page
//! version still match. Promotion to a code page must revoke every writable TLB
//! alias before native execution. Writes to watched pages then leave the native
//! block before the write and retire the version on the shared write path.
//! Once retired, old translations are unreachable and the backing page may be
//! ordinary writable data. A later execute lookup promotes a fresh version and
//! revokes writable aliases again. No old version token is reused within an
//! arena. A runner may retain compiled entries under the new token only after
//! proving that all their instruction-byte dependencies remain unchanged.
//!
//! SoftMMU uses its existing write-permission cache to enforce this contract;
//! HardMMU can protect backing aliases and notify the same version owner after
//! handling the write. Neither invalidation source changes decoding or flags.
//! RAM/device/loader writes must all notify the owner. Reclamation of executable
//! mappings remains separate and is allowed only with no active native stack.

#[cfg(any(target_arch = "aarch64", target_arch = "wasm32"))]
use super::links::PAGE_BYTES;
#[cfg(any(target_arch = "aarch64", target_arch = "wasm32"))]
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::Cell;

/// Only bytes used by translated instructions or negative-cache decisions are
/// dependencies. Other bytes can become data or code later. Record new spans
/// from the current qualified backing bytes, never from an earlier page image.
/// Native publication starts recording after the first invalidation; Wasm
/// records before deferred publication so late results can be validated.
/// The cache owner bounds the number of snapshots and charges their storage.
#[cfg(any(target_arch = "aarch64", target_arch = "wasm32"))]
pub(crate) struct CodeContent {
    bytes: [u8; PAGE_BYTES],
    covered: [u64; PAGE_BYTES / 64],
    pub(crate) end: usize,
}

#[cfg(any(target_arch = "aarch64", target_arch = "wasm32"))]
impl CodeContent {
    #[cfg(target_arch = "aarch64")]
    pub(crate) fn clear(&mut self) {
        self.covered.fill(0);
        self.end = 0;
    }
    // Keep the large initialization temporary off the invocation's stack.
    #[inline(never)]
    pub(crate) fn new() -> Box<Self> {
        Box::new(Self {
            bytes: [0; PAGE_BYTES],
            covered: [0; PAGE_BYTES / 64],
            end: 0,
        })
    }

    pub(crate) fn record(&mut self, offset: usize, bytes: &[u8]) {
        let end = offset + bytes.len();
        self.bytes[offset..end].copy_from_slice(bytes);
        self.end = self.end.max(end);
        // Decoded spans and negative lookahead are nonempty and page-bounded.
        for word in offset / 64..=(end - 1) / 64 {
            let start = word * 64;
            let low = offset.max(start) - start;
            let high = end.min(start + 64) - start;
            self.covered[word] |= (u64::MAX >> (64 - (high - low))) << low;
        }
    }

    pub(crate) fn matches(&self, current: &[u8]) -> bool {
        for (word, &mask) in self.covered[..self.end.div_ceil(64)].iter().enumerate() {
            let start = word * 64;
            if mask == u64::MAX {
                if self.bytes[start..start + 64] != current[start..start + 64] {
                    return false;
                }
            } else {
                let mut remaining = mask;
                while remaining != 0 {
                    let at = start + remaining.trailing_zeros() as usize;
                    if self.bytes[at] != current[at] {
                        return false;
                    }
                    remaining &= remaining - 1;
                }
            }
        }
        true
    }
}

pub struct CodePages {
    epoch: u64,
    // The most recent notification retired exactly this one backing page.
    // MAX denotes multiple pages or a clear; consumers must also match the
    // immediately preceding epoch before using this narrower notification.
    retired_page: usize,
    // Even means unwatched, odd means promoted. The first write retires that
    // version; further data writes need no notification until execution returns.
    // Keeping promotion monotonic prevents a reused page reviving an old block.
    // Cell keeps published raw version pointers valid across notifications.
    // The allocation never moves or grows; execution and mutation are exclusive.
    versions: Vec<Cell<u64>>,
}

impl CodePages {
    pub fn new(bytes: usize) -> Self {
        Self {
            epoch: 0,
            retired_page: usize::MAX,
            versions: alloc::vec![Cell::new(0); bytes.div_ceil(4096)],
        }
    }

    /// Returns (version, first promotion). The caller revokes old writable
    /// translations when promotion is true, before executing the compiled code.
    pub fn watch(&mut self, page: usize) -> (u64, bool) {
        let version = &self.versions[page];
        let promoted = version.get() & 1 == 0;
        if promoted {
            version.set(
                version
                    .get()
                    .checked_add(1)
                    .expect("JIT code version exhausted"),
            );
        }
        (version.get(), promoted)
    }

    pub fn watched(&self, page: usize) -> bool {
        self.versions[page].get() & 1 != 0
    }

    /// Changes only when a published code version is retired. Native writes
    /// cannot touch watched pages, so one check before entry covers a chain.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Stable version cell when exactly one notification since the supplied
    /// epoch retired exactly one page. Older observers and bulk retirement
    /// must inspect all their published versions. This does not consume state.
    pub fn single_retired_version(&self, previous_epoch: u64) -> Option<*const u64> {
        (previous_epoch.checked_add(1) == Some(self.epoch) && self.retired_page != usize::MAX)
            .then(|| self.version_ptr(self.retired_page))
    }

    fn retire_epoch(&mut self) {
        self.epoch = self.epoch.checked_add(1).expect("JIT code epoch exhausted");
    }

    /// Release watches only after every native block and link has been retired.
    /// A page reused as data can then regain a writable translation. The next
    /// promotion must revoke its aliases again. Keep the allocation stable so
    /// reclamation does not replace the version owner or its published pointers.
    pub fn clear(&mut self) {
        self.retired_page = usize::MAX;
        self.retire_epoch();
        for version in &self.versions {
            version.set(0);
        }
    }

    /// Stable until this owner is dropped. Clear all native links before that
    /// happens; never modify the owner while generated code is running.
    pub fn version_ptr(&self, page: usize) -> *const u64 {
        self.versions[page].as_ptr()
    }

    /// A validated backing range is about to change. The notification may
    /// conservatively precede an unchanged-value write or a failed device read.
    pub fn write(&mut self, start: usize, len: usize) {
        if len == 0 {
            return;
        }
        let mut retired = false;
        let mut retired_page = usize::MAX;
        let first = start >> 12;
        for (offset, version) in self.versions[first..=((start + len - 1) >> 12)]
            .iter()
            .enumerate()
        {
            if version.get() & 1 != 0 {
                retired_page = if retired { usize::MAX } else { first + offset };
                retired = true;
                version.set(
                    version
                        .get()
                        .checked_add(1)
                        .expect("JIT code version exhausted"),
                );
            }
        }
        if retired {
            self.retired_page = retired_page;
            self.retire_epoch();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watched_aliases_and_ranges_invalidate_only_covered_pages() {
        let mut pages = CodePages::new(4 * 4096);
        pages.write(0, 4096);
        let (old0, promoted0) = pages.watch(0);
        let (old1, promoted1) = pages.watch(1);
        let (old3, promoted3) = pages.watch(3);
        assert!(promoted0 && promoted1 && promoted3);
        assert_eq!(pages.watch(0), (old0, false));
        pages.write(4094, 4100);
        assert!(!pages.watched(0));
        assert!(!pages.watched(1));
        pages.write(4094, 4100); // Reusing data cannot revive the retired version.
        let (new0, promoted0) = pages.watch(0);
        let (new1, promoted1) = pages.watch(1);
        assert!(promoted0 && promoted1);
        assert_ne!(new0, old0);
        assert_ne!(new1, old1);
        assert!(pages.watch(2).1);
        assert_eq!(pages.watch(3), (old3, false));
    }
}
