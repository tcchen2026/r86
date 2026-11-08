//! Deferred native paths share one compact vector per compiled block.
//! These records replace separately allocated emitter closures; their order
//! remains the order in which the hot emitter requested them.

use super::a64::{Assembler, Label, Reg};
#[cfg(any(test, feature = "jit-profile"))]
use super::frame::{FRAME, Frame};
use super::frame::{PC, RETURN_TOP};
#[cfg(any(test, feature = "jit-profile"))]
use core::mem::offset_of;

pub(super) enum Path {
    Recheck {
        entry: Label,
        matched: Label,
        miss: Label,
        linear: Reg,
        write: bool,
    },
    ReturnStack {
        entry: Label,
        resume: Label,
        overflow: bool,
    },
    Edge {
        entry: Label,
        target: u32,
        miss: Label,
    },
}

impl Path {
    pub(super) fn emit(self, a: &mut Assembler) {
        match self {
            Self::Recheck {
                entry,
                matched,
                miss,
                linear,
                write,
            } => {
                super::soft::emit_recheck_tail(a, entry, matched, miss, linear, write);
            }
            Self::ReturnStack {
                entry,
                resume,
                overflow,
            } => {
                a.bind(entry);
                a.and_high_bits(RETURN_TOP, RETURN_TOP, 12, true);
                #[cfg(any(test, feature = "jit-profile"))]
                if overflow {
                    let native = Reg::new(6);
                    a.load_store(
                        true,
                        native,
                        FRAME,
                        offset_of!(Frame, call_overflows) as u32,
                        3,
                    );
                    a.add_sub_imm(native, native, 1, true, false, false);
                    a.load_store(
                        false,
                        native,
                        FRAME,
                        offset_of!(Frame, call_overflows) as u32,
                        3,
                    );
                }
                #[cfg(not(any(test, feature = "jit-profile")))]
                let _ = overflow;
                a.b(resume);
            }
            Self::Edge {
                entry,
                target,
                miss,
            } => {
                a.bind(entry);
                a.mov_imm(PC, target.into(), false);
                a.b(miss);
            }
        }
    }
}
