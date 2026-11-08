#!/bin/sh
# Observe real firmware allocation through the Linux PCI core and VGA driver.
set -eu
ok=0
trap 'if [ "$ok" != 1 ]; then echo R86_PCI_BOOT_FAIL; poweroff -f; fi' EXIT
vga=/sys/bus/pci/devices/0000:00:02.0
ide=/sys/bus/pci/devices/0000:00:01.1
test "$(cat "$vga/vendor")" = 0x1234
test "$(cat "$vga/device")" = 0x1111
test "$(cat "$ide/device")" = 0x7010
command=$(od -An -tu2 -j4 -N2 "$vga/config")
status=$(od -An -tu2 -j6 -N2 "$vga/config")
pin=$(od -An -tu1 -j61 -N1 "$ide/config")
secondary=$(od -An -tu2 -j66 -N2 "$ide/config")
test "$((command & 3))" -eq 3
test "$((command & 65532))" -eq 0
test "$((status & 16))" -eq 0
test "$((pin))" -eq 0
test "$((secondary & 32768))" -ne 0
echo R86_PCI_VGA_CONFIG
od -An -v -tx1 -N64 "$vga/config"
echo R86_PCI_VGA_RESOURCE
cat "$vga/resource"
echo R86_PCI_IDE_CONFIG
od -An -v -tx1 -N72 "$ide/config"
echo R86_PCI_IDE_RESOURCE
cat "$ide/resource"
mkdir -p /mnt/r86-pci-media
mount -t iso9660 -o ro /dev/sr0 /mnt/r86-pci-media
test -s /mnt/r86-pci-media/boot/vmlinuz
umount /mnt/r86-pci-media
ok=1
echo R86_PCI_BOOT_OK
