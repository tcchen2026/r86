#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraceLayer {
    Architectural,
    Implementation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ArchEventKind {
    Instruction = 1,
    MemoryRead = 2,
    MemoryWrite = 3,
    IoRead = 4,
    IoWrite = 5,
    Exception = 6,
    Interrupt = 7,
    Control = 8,
    Time = 9,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ImplEventKind {
    SoftTranslate = 0x100,
    ShadowFill = 0x101,
    ShadowInvalidate = 0x102,
    NativeAttempt = 0x103,
    NativeAbort = 0x104,
    TlbInvalidate = 0x105,
    KvmExit = 0x106,
    Pool = 0x107,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraceEventKind {
    Arch(ArchEventKind),
    Impl(ImplEventKind),
}

impl TraceEventKind {
    pub const fn layer(self) -> TraceLayer {
        match self {
            Self::Arch(_) => TraceLayer::Architectural,
            Self::Impl(_) => TraceLayer::Implementation,
        }
    }

    pub const fn code(self) -> u16 {
        match self {
            Self::Arch(kind) => kind as u16,
            Self::Impl(kind) => kind as u16,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraceEvent {
    pub index: u64,
    pub layer: TraceLayer,
    pub kind: u16,
    pub flags: u16,
    pub vcpu: u16,
    pub reserved: u16,
    pub a: u64,
    pub b: u64,
    pub c: u64,
}

impl TraceEvent {
    pub const fn new(
        index: u64,
        kind: TraceEventKind,
        flags: u16,
        vcpu: u16,
        a: u64,
        b: u64,
        c: u64,
    ) -> Self {
        Self {
            index,
            layer: kind.layer(),
            kind: kind.code(),
            flags,
            vcpu,
            reserved: 0,
            a,
            b,
            c,
        }
    }

    pub const fn arch(
        index: u64,
        kind: ArchEventKind,
        flags: u16,
        vcpu: u16,
        a: u64,
        b: u64,
        c: u64,
    ) -> Self {
        Self::new(index, TraceEventKind::Arch(kind), flags, vcpu, a, b, c)
    }

    pub const fn implementation(
        index: u64,
        kind: ImplEventKind,
        flags: u16,
        vcpu: u16,
        a: u64,
        b: u64,
        c: u64,
    ) -> Self {
        Self::new(index, TraceEventKind::Impl(kind), flags, vcpu, a, b, c)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TraceCoverage {
    pub arch_events: u64,
    pub impl_events: u64,
    pub soft_translates: u64,
    pub shadow_fills: u64,
    pub shadow_invalidations: u64,
    pub native_attempts: u64,
    pub native_aborts: u64,
    pub tlb_invalidations: u64,
    pub kvm_exits: u64,
}

impl TraceCoverage {
    pub fn observe(&mut self, event: TraceEvent) {
        match event.layer {
            TraceLayer::Architectural => self.arch_events = self.arch_events.wrapping_add(1),
            TraceLayer::Implementation => self.impl_events = self.impl_events.wrapping_add(1),
        }
        match event.kind {
            x if x == ImplEventKind::SoftTranslate as u16 => {
                self.soft_translates = self.soft_translates.wrapping_add(1)
            }
            x if x == ImplEventKind::ShadowFill as u16 => {
                self.shadow_fills = self.shadow_fills.wrapping_add(1)
            }
            x if x == ImplEventKind::ShadowInvalidate as u16 => {
                self.shadow_invalidations = self.shadow_invalidations.wrapping_add(1)
            }
            x if x == ImplEventKind::NativeAttempt as u16 => {
                self.native_attempts = self.native_attempts.wrapping_add(1)
            }
            x if x == ImplEventKind::NativeAbort as u16 => {
                self.native_aborts = self.native_aborts.wrapping_add(1)
            }
            x if x == ImplEventKind::TlbInvalidate as u16 => {
                self.tlb_invalidations = self.tlb_invalidations.wrapping_add(1)
            }
            x if x == ImplEventKind::KvmExit as u16 => {
                self.kvm_exits = self.kvm_exits.wrapping_add(1)
            }
            _ => {}
        }
    }

    pub const fn saw_hardmmu_path(self) -> bool {
        self.shadow_fills != 0 || self.native_attempts != 0 || self.native_aborts != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_coverage_counts_arch_and_impl_events_separately() {
        let mut coverage = TraceCoverage::default();

        coverage.observe(TraceEvent::arch(
            1,
            ArchEventKind::Instruction,
            0,
            0,
            0x100,
            0,
            0,
        ));
        coverage.observe(TraceEvent::implementation(
            2,
            ImplEventKind::ShadowFill,
            0,
            0,
            0x200,
            0x300,
            0,
        ));

        assert_eq!(coverage.arch_events, 1);
        assert_eq!(coverage.impl_events, 1);
        assert_eq!(coverage.shadow_fills, 1);
        assert!(coverage.saw_hardmmu_path());
    }
}
