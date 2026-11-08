#!/bin/sh
# Run inside TinyCore via --initrd-file and --bootlocal-command; see
# design/VALIDATION.md. This uses Linux's real CD-ROM/block/filesystem drivers.
# All writes are to guest RAM; the mounted ISO remains read-only.
set -eu
ok=0
trap 'if [ "$ok" != 1 ]; then echo R86_ATAPI_MEDIA_FAIL; poweroff -f; fi' EXIT
dev=/dev/sr0
point=/mnt/r86-media
mkdir -p "$point"
mount -t iso9660 -o ro "$dev" "$point"
dd if="$point/boot/vmlinuz" of=/tmp/r86-media-before bs=2048 count=64
test "$(wc -c </tmp/r86-media-before)" -eq 131072
umount "$point"
/bin/busybox eject "$dev"
if mount -t iso9660 -o ro "$dev" "$point"; then
    umount "$point"
    echo 'An ejected CD must not mount successfully'
    exit 1
fi
echo R86_ATAPI_MEDIA_EMPTY
/bin/busybox eject -t "$dev"
mount -t iso9660 -o ro "$dev" "$point"
dd if="$point/boot/vmlinuz" of=/tmp/r86-media-after bs=2048 count=64
cmp /tmp/r86-media-before /tmp/r86-media-after
md5sum /tmp/r86-media-after
umount "$point"
ok=1
echo R86_ATAPI_MEDIA_OK
