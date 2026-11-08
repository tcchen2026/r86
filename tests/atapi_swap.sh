#!/bin/sh
# Driven by qt_linux_observes_live_iso_changes: host changes media only after
# these serial rendezvous, then sends a newline. All guest writes stay in RAM.
set -eu
ok=0
trap 'if [ "$ok" != 1 ]; then echo R86_ATAPI_SWAP_FAIL; poweroff -f; fi' EXIT
dev=/dev/sr0
point=/mnt/r86-swap
mkdir -p "$point"
wait_host() {
    echo "$1"
    read -r ack </dev/ttyS0
}
check_disc() {
    mount -t iso9660 -o ro "$dev" "$point"
    test "$(cat "$point/boot/isolinux/isolinux.cfg")" = "$1"
}
wait_host R86_ATAPI_SWAP_INSERT
check_disc R86_DISC_A
wait_host R86_ATAPI_SWAP_LOCKED
test "$(cat "$point/boot/isolinux/isolinux.cfg")" = R86_DISC_A
umount "$point"
wait_host R86_ATAPI_SWAP_REPLACE
check_disc R86_DISC_B
umount "$point"
wait_host R86_ATAPI_SWAP_EJECT
if mount -t iso9660 -o ro "$dev" "$point"; then
    umount "$point"
    echo 'An empty drive must not mount successfully'
    exit 1
fi
wait_host R86_ATAPI_SWAP_REINSERT
check_disc R86_DISC_A
umount "$point"
ok=1
echo R86_ATAPI_SWAP_OK
poweroff -f
