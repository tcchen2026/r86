USB contract
============

Opt in with --usb-tablet, usb-tablet = true in CLI TOML, or the same key in
handheld vm.toml. Default is absent. PCI 00:01.2 is PIIX3 UHCI 8086:7020,
revision 1, class 0c0300, a 32-byte I/O BAR4 and two root ports. BAR sizing
uses the QEMU-compatible 32-bit mask (SeaBIOS rejects the physical PIIX3
16-bit mask); high assigned I/O addresses do not alias x86 ports. Port 1 has
a full-speed HID tablet; port 2 is empty. No hub or high-speed controller is
needed. Revision 0 causes Windows 2000 to reject the controller (Code 10)
before creating its frame schedule.

Machine::inject_usb_tablet uses X/Y 0..32767, left/right/middle button bits
0/1/2 and signed wheel -127..127 (positive up). Events enter on the machine
thread at a slice boundary. Motion coalesces; a bounded 64-report queue keeps
button transitions. script/ws_cli.py owns the WebSocket and replay contract.
Lead owns SDL, handheld and browser pointer UI wiring.

Timing and schedule
-------------------
UHCI runs on the selected machine clock. The default 12000 USB clock periods
per frame equal 1 ms; SOFMOD changes that to (11936 + SOFMOD)/12 MHz. Integer
oscillator deadlines carry the fractional remainder. FRNUM has 11 bits and
its low ten bits index the 4 KiB frame list. Transactions, descriptor updates
and IOC/short-packet status become visible at frame boundaries. There is no
host timer, USB thread, physical wire/CRC simulation or subframe visibility.

The walker follows horizontal QHs and vertical TDs, including depth/breadth,
terminate, inactive IOC, NAK, error count, short packets and Windows bandwidth
reclamation loops. A frame is bounded by 256 descriptor visits and 1280 data
bytes. QH cycles stop when no transaction has progressed. An idle schedule
is cached with its observed descriptor words; each frame validates RAM before
skipping the walk. Guest edits and new/idle-expired HID reports therefore wake
it without RAM write instrumentation. This removes repeated NAK descriptor
walks, not the 1 ms frame-list read/validation. With bus mastering disabled,
FRNUM catches up arithmetically and no DMA event is scheduled.

All DMA windows must fit RAM and exclude the VGA/ROM hole 0xa0000..0xfffff.
Invalid windows set Host System Error and halt; invalid PID/oversized TDs set
Host Controller Process Error. DMA bypasses A20 and notifies JIT code-page
versions before committing RAM. There is no DMA
to mapped framebuffer/device memory. Data toggle/wire retry is not simulated.

Interrupts and tablet
---------------------
INTD -> PIRQD -> IRQ11 matches SeaBIOS slot/pin routing and DSDT _PRT. ELCR
marks it level-triggered; PIC input is the OR of UHCI and NIC. PCI LEGSUP
bit 13 gates PIRQD. No SMI/legacy keyboard trap is implemented. PCI interrupt
line readback is writable; physical board routing remains fixed.

EP0 has 8-byte packets and USB chapter 9 descriptor/address/configuration,
status/features/interface operations. SET_ADDRESS takes effect after status.
The HID interface is nonboot absolute pointer with a 6-byte report, interrupt
EP81 (8-byte maximum, 10 ms interval), three buttons and wheel. GET_REPORT,
GET/SET_IDLE and report/HID descriptors work; boot-only GET/SET_PROTOCOL and
unsupported requests stall. Root reset clears address/configuration; no
host-side hotplug UI exists. Suspend blocks transfers; enabled remote wakeup
latches resume. SWDBG stepping, physical resume/EOP delays, bus contention,
USB legacy SMI and electrical low-speed devices are outside this batch.

Provenance
----------
uhci.rs adapts QEMU hw/usb/hcd-uhci.c. tablet.rs adapts hw/usb/core.c control
stages and hw/usb/dev-hid.c + hw/input/hid.c HID layout/behavior. These files
are MIT at bb779689554ca5e69e0ac0bb56f90c41c64a80bb. Full original notices are
in the adapted Rust headers and qemu-license.txt; ports/share.sh ships them.
No QEMU desc.c/bus.c, GPL/LGPL controller or USB device code was adapted.
Descriptor constants/chapter 9 handling use the specifications and the above
MIT HID description. tiny386 and v86 were checked first; neither has USB.

Specifications consulted: Intel UHCI 1.1, Intel 82371SB PIIX3 USB function,
USB 1.1 chapter 9, USB HID 1.11 and HID Usage Tables 1.12 (Generic Desktop,
buttons, wheel). Local source URLs and guest evidence are in the Intern's
handoff to Lead, tmp/usb/handoff.txt (outside Git).
