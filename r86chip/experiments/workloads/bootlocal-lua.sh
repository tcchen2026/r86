#!/bin/sh
set -eu
exec >/dev/ttyS0 2>&1
uname -a
echo R86_CHIP_TINYCORE_OK
export LD_LIBRARY_PATH=/usr/local/lib
/usr/local/bin/lua /opt/r86chip-workload.lua
test "$(cat /tmp/r86chip-shell-data)" = shell-and-lua
echo R86_CHIP_LUA_OK
