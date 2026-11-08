//! Flat-RAM native fixture runner for the RTL suite. The ISS, not this harness,
//! decodes instructions. Initialized RTL register observations and all RAM are
//! compared; uninitialized physical RF contents are not architectural promises.
use super::{
    memory::Memory,
    rv32::{Cause, Core},
};

pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() != 2 {
        return Err("native requires one flat RAM image; initial PC is 0x1000".into());
    }
    let bytes = std::fs::read(&args[1])?;
    let mut ram = Memory::new(bytes.len());
    ram.write(0, &bytes)?;
    let mut core = Core::new(0x1000);
    let t = core.run(&mut ram, 200_000)?;
    let cause = match t.cause {
        Cause::InstructionMisaligned => 0,
        Cause::InstructionAccess => 1,
        Cause::IllegalInstruction => 2,
        Cause::Breakpoint => 3,
        Cause::LoadMisaligned => 4,
        Cause::LoadAccess => 5,
        Cause::StoreMisaligned => 6,
        Cause::StoreAccess => 7,
        Cause::Ecall => 11,
        Cause::GuestService => 12,
        Cause::StepLimit => return Err("native fixture exceeded instruction limit".into()),
    };
    // The debug RTL port reports the EBREAK PC as tval; the ISS trap value for
    // ECALL/EBREAK is zero. Normalize only that harness representation here.
    let value = if t.cause == Cause::Breakpoint {
        t.pc
    } else {
        t.value
    };
    let hash = ram.ram.iter().fold(0xcbf29ce484222325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    println!(
        "{{\"pc\":{},\"cause\":{},\"value\":{},\"registers\":{:?},\"ram_fnv64\":\"{hash:016x}\",\"retired_native\":{}}}",
        t.pc,
        cause,
        value,
        core.register_banks(),
        core.retired_steps
    );
    Ok(())
}
