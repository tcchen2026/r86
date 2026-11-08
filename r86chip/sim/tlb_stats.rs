//! Opt-in host observations of the guest translation table, not chip state.
//!
//! Count encoding-valid attempts before service/native-bus checks. A `hit`
//! means a qualified translation, not a retired load: an unavailable native
//! backing bus or missing service vector may still trap. Illegal encodings are
//! excluded. XTLOOK/XTFILL cannot fault after this observation; XLD/XSW can.
//! These counters never choose an execution path, grant permissions, install a
//! mapping, or change the firmware's measured native instruction count.
use super::GuestMapping;

#[derive(Default)]
pub struct TlbStats {
    // Rows: read, write, fetch, RMW. Columns follow probe_outcomes in json().
    pub(super) lookup: [[u64; 7]; 4],
    pub(super) memory: [[u64; 7]; 4],
    // Rows: proposed R/W/X bits. Columns follow fill_outcomes in json().
    pub(super) fill: [[u64; 6]; 8],
    // Old rights -> new rights, for equal linear AND native pages, or a tag
    // collision respectively. Equal mappings can still revoke permissions.
    pub(super) same_mapping_rights: [[u64; 8]; 8],
    pub(super) collision_rights: [[u64; 8]; 8],
    // Each row is a histogram of valid entries removed (0..=20). Counting the
    // zero column matters: a flush instruction need not evict anything.
    pub(super) invalidation: [[u64; 21]; 4],
}

impl TlbStats {
    /// `early_reason` comes from XLD's architectural priority checks, or from
    /// XTLOOK's alignment check. Only a table rejection is further classified.
    pub(super) fn probe(
        &mut self,
        memory: bool,
        intent: usize,
        entry: Option<GuestMapping>,
        linear: u32,
        early_reason: u32,
    ) {
        let required = [1, 2, 4, 3][intent];
        let outcome = match early_reason {
            1..=3 => early_reason as usize,
            _ => match entry {
                None => 4,
                Some(e) if e.linear_page != linear & !0xfff => 5,
                Some(e) if e.rights & required != required => 6,
                Some(_) => 0,
            },
        };
        if memory {
            self.memory[intent][outcome] += 1;
        } else {
            self.lookup[intent][outcome] += 1;
        }
    }

    pub(super) fn fill(
        &mut self,
        old: Option<GuestMapping>,
        new: Option<GuestMapping>,
        proposed_rights: u32,
    ) {
        let outcome = match (old, new) {
            (_, None) => 0,
            (None, Some(_)) => 1,
            (Some(a), Some(b)) if a.linear_page != b.linear_page => {
                self.collision_rights[a.rights as usize][b.rights as usize] += 1;
                2
            }
            (Some(a), Some(b)) if a.native_page != b.native_page => 3,
            (Some(a), Some(b)) => {
                self.same_mapping_rights[a.rights as usize][b.rights as usize] += 1;
                if a.rights != b.rights { 4 } else { 5 }
            }
        };
        self.fill[proposed_rights as usize][outcome] += 1;
    }

    pub(super) fn invalidate(&mut self, kind: usize, removed: usize) {
        self.invalidation[kind][removed] += 1;
    }

    /// Labels travel with arrays so saved results do not depend on a later
    /// source version. Rust Debug formatting of these integer arrays is JSON.
    pub fn json(&self) -> String {
        format!(
            "{{\n\
             \"scope\":\"encoding-valid attempts; translation hits precede native bus/service checks\",\n\
             \"intents\":[\"read\",\"write\",\"fetch\",\"rmw\"],\n\
             \"probe_outcomes\":[\"hit\",\"context\",\"segment\",\"alignment_or_page_span\",\"empty\",\"tag\",\"rights\"],\n\
             \"xtlook\":{:?},\n\"xld_xsw\":{:?},\n\
             \"fill_outcomes\":[\"rejected_aperture\",\"empty\",\"tag_collision\",\"backing_change\",\"rights_change\",\"identical\"],\n\
             \"fills_by_new_rights\":{:?},\n\
             \"same_mapping_old_to_new_rights\":{:?},\n\
             \"tag_collision_old_to_new_rights\":{:?},\n\
             \"invalidation_kinds\":[\"xtinval\",\"xtflush\",\"xtregion\",\"changed_xgctx\"],\n\
             \"invalidations_by_removed_entries\":{:?}\n}}\n",
            self.lookup,
            self.memory,
            self.fill,
            self.same_mapping_rights,
            self.collision_rights,
            self.invalidation,
        )
    }
}
