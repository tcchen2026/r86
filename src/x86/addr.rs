#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysAddr(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinearAddr(pub u32);

impl PhysAddr {
    #[inline]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[inline]
    pub const fn as_u32(self) -> u32 {
        self.0 as u32
    }

    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl LinearAddr {
    #[inline]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[inline]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}
