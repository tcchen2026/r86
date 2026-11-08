//! Create a DOS VHD with the same kit installer used by the frontend.
#[cfg(feature = "host")]
fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err("usage: dos_disk OUTPUT.vhd SIZE_MIB OS_ID SHARE_DIR".into());
    }
    let size = args[1].parse().map_err(|_| "SIZE_MIB must be an integer")?;
    let mut last_mib = u64::MAX;
    r86::dos_disk::create(
        std::path::Path::new(&args[0]),
        size,
        &args[2],
        std::path::Path::new(&args[3]),
        |done, total| {
            let mib = done / (1024 * 1024);
            if mib != last_mib || done == total {
                eprintln!("{done}/{total} source bytes");
                last_mib = mib;
            }
        },
    )
}

#[cfg(not(feature = "host"))]
fn main() {
    eprintln!("dos_disk requires the host feature");
    std::process::exit(1);
}
