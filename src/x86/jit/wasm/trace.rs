//! Bounded diagnostic history, absent from ordinary builds. Node drains it only
//! between core invocations. These are observations, never executable cache
//! inputs. A full buffer counts dropped rows instead of growing or blocking.
//!
//! Each row is [native_steps, interpreted_steps, kind, a, b, c, d, e, f, g, h].
//! Kind 1: warm/candidate [backing:pc key, count, previous_seen, candidate_at].
//! Kind 2: heat reset [old key, count, last_seen, candidate_at, cause, new key,
//! last_seen_interpreted];
//! causes 1=idle, 2=collision. The clocks are the policy's total guest steps.
//! Kind 3: admission [request, slot, root key, bytes, charge, instructions,
//! functions, cause]; causes 1=delayed, 2=burst, 3=remembered capacity victim.
//! Kind 4: publication [request, slot, source version, function-table base].
//! Kind 5: retirement [request, slot, function entries, cause, charge]; causes
//! 1=capacity, 2=source write, 3=reset, 4=discarded pending source. A pending
//! module may retire without publication. Kind 6: sampled interpretation [pc, cr3,
//! steps, elapsed ns, preceding exit, eligible]. Kind 7: sampled native chain
//! [pc, function, entry, steps, elapsed ns, exit]. Samples have period 256;
//! native chain cost is not attributed to one module's instruction body.
//! Kind 8: compile cost [request, core emission ns, host submission ns]. Host
//! submission does not include later lazy compilation or background tiering.
//!
//! Module censuses are [request, slot, entries, charge, functions, backing,
//! origin]. Entries count generated function entries (including tail calls and
//! unsuccessful guards), not retired guest instructions. The 29-bit count wraps.

use alloc::{string::String, vec::Vec};
use core::fmt::Write;

const CAPACITY: usize = 16384; // 1.375 MiB inside the diagnostic scratch reserve.

pub struct Trace {
    events: Vec<[u64; 11]>,
    dropped: u64,
}

impl Trace {
    pub fn new() -> Self {
        Self {
            events: Vec::with_capacity(CAPACITY),
            dropped: 0,
        }
    }

    pub fn push(&mut self, native: u64, interpreted: u64, kind: u64, data: [u64; 8]) {
        if self.events.len() == CAPACITY {
            self.dropped += 1;
            return;
        }
        let [a, b, c, d, e, f, g, h] = data;
        self.events
            .push([native, interpreted, kind, a, b, c, d, e, f, g, h]);
    }

    pub fn drain(&mut self) -> String {
        let mut out = String::new();
        write!(
            out,
            "{{\"schema\":1,\"sample_period\":256,\"dropped\":{},\"events\":[",
            self.dropped
        )
        .unwrap();
        for (i, row) in self.events.drain(..).enumerate() {
            if i != 0 {
                out.push(',');
            }
            write!(out, "{row:?}").unwrap();
        }
        out.push_str("],\"modules\":[");
        out
    }
}
