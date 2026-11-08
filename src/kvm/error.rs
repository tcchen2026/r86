use std::fmt;
use std::io;

use crate::monitor::abi::AbiError;

pub type Result<T> = std::result::Result<T, KvmError>;

#[derive(Debug)]
pub enum KvmError {
    Io {
        op: &'static str,
        source: io::Error,
    },
    BadApiVersion {
        found: i32,
    },
    MissingCapability {
        cap: u32,
        name: &'static str,
    },
    CapabilityTooSmall {
        cap: u32,
        name: &'static str,
        found: i32,
        required: i32,
    },
    InvalidIpaBits {
        requested: u8,
        min: u8,
        max: u8,
    },
    Layout(AbiError),
    MemslotPlan(&'static str),
    BadArgument(String),
    ParseArgument {
        name: &'static str,
        value: String,
    },
    UnsupportedHost(&'static str),
}

impl KvmError {
    pub fn io(op: &'static str) -> Self {
        Self::Io {
            op,
            source: io::Error::last_os_error(),
        }
    }
}

impl fmt::Display for KvmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "{op}: {source}"),
            Self::BadApiVersion { found } => {
                write!(f, "unsupported KVM API version {found}, expected 12")
            }
            Self::MissingCapability { cap, name } => {
                write!(f, "missing KVM capability {name} ({cap})")
            }
            Self::CapabilityTooSmall {
                cap,
                name,
                found,
                required,
            } => write!(
                f,
                "KVM capability {name} ({cap}) is {found}, required at least {required}"
            ),
            Self::InvalidIpaBits {
                requested,
                min,
                max,
            } => write!(
                f,
                "invalid IPA size request {requested}; valid range is {min}..={max}"
            ),
            Self::Layout(err) => write!(f, "invalid HardMMU ABI layout: {err:?}"),
            Self::MemslotPlan(msg) => write!(f, "invalid KVM memslot plan: {msg}"),
            Self::BadArgument(msg) => write!(f, "{msg}"),
            Self::ParseArgument { name, value } => {
                write!(f, "could not parse {name} value {value:?}")
            }
            Self::UnsupportedHost(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for KvmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<AbiError> for KvmError {
    fn from(value: AbiError) -> Self {
        Self::Layout(value)
    }
}
