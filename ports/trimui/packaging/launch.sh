#!/bin/sh
# MainUI runs this with the screen released and returns when it exits.
# The app and its data (VMs, disk and CD images, the log in /mnt/SDCARD/r86)
# stay on the SD card.
cd "$(dirname "$0")"
data=/mnt/SDCARD/r86
mkdir -p "$data"
echo 1 >/tmp/stay_awake
# The guest runs on one thread; keep both A55 clusters at full speed.
# MainUI's preload.sh restores ondemand afterwards.
for cpu in 0 4; do
    echo performance >/sys/devices/system/cpu/cpu$cpu/cpufreq/scaling_governor
done
./r86-handheld --data "$data" >"$data/r86.log" 2>&1
rm -f /tmp/stay_awake
