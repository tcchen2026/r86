//! Browser-only imports. Binaryen Asyncify suspends the *existing* interpreter
//! stack at `disk_io`; the JS host must not enter any Rust export until it has
//! rewound that stack. There is no guest instruction replay or device retry.
//! Native and monitor builds do not contain these imports or storage variants.
//!
//! The JS host owns external storage. A write completes before returning;
//! failures become ordinary IDE errors. The IndexedDB host will need to wait
//! for transaction completion, not just request success. Byte offsets
//! cross the JS boundary as f64 (exact throughout our LBA28 disk range).

use crate::disk::DiskError;

#[link(wasm_import_module = "browser")]
unsafe extern "C" {
    fn disk_io(drive: u32, operation: u32, offset: f64, bytes: *mut u8, len: usize) -> i32;
    fn monotonic_ms() -> f64;
    fn epoch_ms() -> f64;
}

pub fn now_ms() -> f64 {
    unsafe { monotonic_ms() }
}

pub fn epoch_seconds() -> u64 {
    (unsafe { epoch_ms() } / 1000.0) as u64
}

pub(crate) fn read(drive: u32, offset: u64, bytes: &mut [u8]) -> Result<(), DiskError> {
    result(unsafe { disk_io(drive, 0, offset as f64, bytes.as_mut_ptr(), bytes.len()) })
}

pub(crate) fn write(drive: u32, offset: u64, bytes: &[u8]) -> Result<(), DiskError> {
    // The write import only reads this buffer. Asyncify preserves its lifetime
    // until the transaction finishes, including across memory.grow.
    result(unsafe {
        disk_io(
            drive,
            1,
            offset as f64,
            bytes.as_ptr().cast_mut(),
            bytes.len(),
        )
    })
}

pub(crate) fn flush(drive: u32) -> Result<(), DiskError> {
    result(unsafe { disk_io(drive, 2, 0.0, core::ptr::null_mut(), 0) })
}

fn result(code: i32) -> Result<(), DiskError> {
    if code == 0 {
        Ok(())
    } else {
        Err(DiskError::BrowserIo)
    }
}
