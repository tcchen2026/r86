//! Host entry for the current RV32/Xr86 instruction interpreter.
#[cfg(test)]
extern crate alloc;
#[cfg(test)]
mod b1_tests;
mod cache_estimate;
mod machine;
mod memory;
mod native;
mod rv32;
#[cfg(test)]
mod rv32_tests;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--help") => {
            println!(
                "r86chip-sim machine FIRMWARE.elf IMAGE_DIRECTORY RV_STEPS WALL_SECONDS [--profile] [--dump-guest] [--tlb-stats] [--cache-stats]\n\
                 r86chip-sim native RAM_IMAGE.bin\n\
                 Executes RV32IM + Zifencei + Zba + Xr86 firmware and generated code.\n\
                 Functional ISA simulation; instruction counts are not hardware cycles.\n\
                 Use r86chip/experiments/tinycore.py --engine iss to prepare boot inputs."
            );
            Ok(())
        }
        Some("machine") => machine::run(&args),
        Some("native") => native::run(&args),
        Some(command) => Err(format!("unknown command {command}; use --help").into()),
    }
}
fn main() {
    if let Err(e) = run() {
        eprintln!("r86chip-sim: {e}");
        std::process::exit(1);
    }
}
