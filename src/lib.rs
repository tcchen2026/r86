//! Rust i386 machine and execution backends.
//!
//! The default `host` build includes the embeddable interpreter runtime.
//! `machine` without default features builds the same x86 implementation for
//! bare metal (no_std). Frontends and platform backends are selected at build time.

#![cfg_attr(not(feature = "host"), no_std)]

#[cfg(any(feature = "machine", feature = "jit"))]
extern crate alloc;

pub mod x86;

#[cfg(all(feature = "host", target_vendor = "apple"))]
mod host_counters;

#[cfg(all(feature = "web-frontend", target_arch = "wasm32"))]
pub mod browser;

#[cfg(feature = "r86chip")]
#[path = "../r86chip/jit/mod.rs"]
pub mod chipjit;

#[cfg(feature = "r86chip")]
#[path = "../r86chip/interp/mod.rs"]
pub mod chipinterp;

#[cfg(feature = "machine")]
mod acpi;

#[cfg(feature = "apple")]
#[path = "frontend/apple/mod.rs"]
pub mod apple;
// Frontend modules shared with the binaries name the crate `r86`.
#[cfg(feature = "apple")]
extern crate self as r86;
#[cfg(feature = "host")]
#[path = "frontend/audio_output.rs"]
pub mod audio_output;
#[cfg(feature = "machine")]
pub mod devices;
#[cfg(feature = "machine")]
pub mod disk;
#[cfg(feature = "host")]
#[path = "frontend/dos_disk.rs"]
pub mod dos_disk;
#[cfg(feature = "host")]
#[path = "frontend/fat.rs"]
pub mod fat;
#[cfg(feature = "host")]
pub mod initrd_overlay;
#[cfg(feature = "host")]
pub mod iso9660;
#[cfg(feature = "host")]
pub mod linux_boot;
#[cfg(feature = "machine")]
pub mod machine;
#[cfg(feature = "host")]
#[path = "frontend/network.rs"]
pub mod network;
#[cfg(feature = "host")]
pub mod remote_cd;
#[cfg(feature = "machine")]
#[path = "pc_devices.rs"]
mod run_devices;
#[cfg(feature = "machine")]
pub use run_devices::FirmwareDisplay;
