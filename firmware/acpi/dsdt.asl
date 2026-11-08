// r86's single-CPU, legacy PIC PC. No APIC, sleep/resume or hotplug is advertised.
// Rebuild: iasl -p firmware/acpi/dsdt firmware/acpi/dsdt.asl
DefinitionBlock ("", "DSDT", 1, "R86   ", "R86PC   ", 1)
{
    Name (_S5, Package (4) { Zero, Zero, Zero, Zero })
    Scope (_PR)
    {
        Processor (CPU0, 0, 0, 0) {}
    }
    Scope (_SB)
    {
        Device (PCI0)
        {
            Name (_HID, EisaId ("PNP0A03"))
            Name (_UID, Zero)
            Name (_BBN, Zero)
            Name (_CRS, ResourceTemplate ()
            {
                WordBusNumber (ResourceProducer, MinFixed, MaxFixed, PosDecode,
                    0, 0, 0, 0, 1)
                IO (Decode16, 0xCF8, 0xCF8, 1, 8)
                DWordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
                    0, 0, 0xCF7, 0, 0xCF8)
                DWordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
                    0, 0xD00, 0xFFFF, 0, 0xF300)
                // SeaBIOS allocates PCI BARs downwards from FEC00000 (see
                // vendor/seabios/src/fw/pciinit.c). The current VGA BAR lands
                // at FD000000; excluding it makes Windows' bus-address
                // translation reject the linear framebuffer. The root window
                // must include the whole allocation, stopping before IOAPIC.
                // Bochs' acpi-dsdt.dsl uses the same upper MMIO boundary.
                DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed,
                    NonCacheable, ReadWrite, 0, 0xE0000000, 0xFEBFFFFF, 0, 0x1EC00000)
            })
            Name (_PRT, Package () { Package () { 0x0001FFFF, Zero, Zero, 14 } })
            Device (ISA)
            {
                Name (_ADR, 0x00010000)
                Device (PMIO)
                {
                    Name (_HID, EisaId ("PNP0C02"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x600, 0x600, 1, 12)
                        IO (Decode16, 0xB2, 0xB2, 1, 1)
                    })
                }
                Device (PIC)
                {
                    Name (_HID, EisaId ("PNP0000"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x20, 0x20, 1, 2)
                        IO (Decode16, 0xA0, 0xA0, 1, 2)
                        IRQNoFlags () { 2 }
                    })
                }
                Device (PIT)
                {
                    Name (_HID, EisaId ("PNP0100"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x40, 0x40, 1, 4)
                        IRQNoFlags () { 0 }
                    })
                }
                Device (RTC)
                {
                    Name (_HID, EisaId ("PNP0B00"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x70, 0x70, 1, 2)
                        IRQNoFlags () { 8 }
                    })
                }
                Device (KBD)
                {
                    Name (_HID, EisaId ("PNP0303"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x60, 0x60, 1, 1)
                        IO (Decode16, 0x64, 0x64, 1, 1)
                        IRQNoFlags () { 1 }
                    })
                }
                Device (MOU)
                {
                    Name (_HID, EisaId ("PNP0F13"))
                    Name (_CRS, ResourceTemplate () { IRQNoFlags () { 12 } })
                }
                Device (COM1)
                {
                    Name (_HID, EisaId ("PNP0501"))
                    Name (_UID, One)
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x3F8, 0x3F8, 1, 8)
                        IRQNoFlags () { 4 }
                    })
                }
            }
        }
    }
}
