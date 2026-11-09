//! Rust i386 machine and execution backends.
//!
//! The default `host` build includes the embeddable interpreter runtime.
//! `monitor` without default features builds the same x86 implementation for
//! bare-metal EL1. Frontends and platform backends are selected at build time.

#![cfg_attr(not(feature = "host"), no_std)]

#[cfg(any(feature = "machine", feature = "jit"))]
extern crate alloc;

pub mod x86;

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
#[cfg(feature = "machine")]
#[path = "pc_devices.rs"]
mod run_devices;
#[cfg(feature = "machine")]
pub use run_devices::FirmwareDisplay;

#[cfg(all(feature = "kvm", target_os = "linux"))]
pub mod kvm;
#[cfg(feature = "monitor")]
pub mod monitor;
