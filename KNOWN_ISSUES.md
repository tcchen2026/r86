# Known issues

Defects that are known and not fixed yet. Each entry gives the trigger, the
effect and the code path. Delete an entry in the commit that fixes it.

## Deferred: no known guest depends on them

- **Page-table accessed/dirty store into device memory stops the machine.**
  If a guest's page directory or page table lies in device memory or ROM
  (VGA window, linear framebuffer, APIC pages, BIOS area), the page walk's
  A/D update is refused by `HostBus::check_write_range` (src/machine.rs) and
  the machine stops with an emulator error. No host hang, no memory-safety
  effect.
- **IDE: reading the data port during a write transfer ends the write.**
  With DRQ set for a sector write, an IN from the data register reaches
  `IdeChannel::read_data_u8` and `finish_data_if_drained`
  (src/pc_devices.rs), which clear DRQ and drop the transfer. The sector is
  not written and no error is reported.
- **FXSAVE/FXRSTOR test alignment on the segment offset, not the linear
  address.** `aligned_fxsave_memory` (src/x86/interp.rs). With a data
  segment whose base is not a multiple of 16, a misaligned operand is
  accepted and an aligned one raises #GP(0). Flat segments are unaffected.
