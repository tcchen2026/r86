#!/bin/sh
# The HDMI desktop's panel and file manager (wallpaper and icons) stop while r86
# runs, for their memory, and start again after it; adapted from VibeOffice's
# desktop/cardputer/helpers.sh. The launcher wrapper runs `stop PID` (PID: its
# own process, which exec keeps for r86-handheld). A watcher (`after PID`), a
# unit of the user's systemd outside the launcher's process tree, starts them
# again once r86 has ended, however it ended. Only what ran is started again;
# a stop waits for a resume still going (a quick start again). labwc, the
# launcher, the session bus, the sound server (the launcher's sounds; r86's
# sound) and the desktop portals stay.
#   helpers.sh stop [PID] | resume | after PID
set -u
state=${XDG_RUNTIME_DIR:-/tmp}/r86-helpers
# each under lwrespawn, which starts it again while labwc runs
programs='/usr/bin/wf-panel-pi /usr/bin/pcmanfm-pi'

# one stop or resume at a time (fd 9; what they start goes without it, or it
# would hold the lock for good)
lock() { exec 9> "$state.lock" && flock 9; }

stop() {
  lock
  mkdir "$state" 2>/dev/null || return 0   # stopped already, by a run still going
  n=0
  for p in $programs; do
    for w in $(pgrep -u "$(id -u)" -f "lwrespawn $p\$"); do
      n=$((n + 1))
      echo "$p" > "$state/program.$n"
      cp "/proc/$w/environ" "$state/environ.$n"
      kids=$(pgrep -P "$w")
      kill "$w"   # the wrapper first, or it starts the program again
      for k in $kids; do pkill -P "$k"; kill "$k"; done
    done
  done
}

resume() {
  lock
  [ -d "$state" ] || return 0
  for f in "$state"/program.*; do
    [ -f "$f" ] || continue
    xargs -0 -a "$state/environ.${f##*.}" "$0" spawn "$(cat "$f")" </dev/null >/dev/null 2>&1 9>&-
  done
  rm -rf "$state"
}

# PROGRAM again under lwrespawn, in the session's environment as it was (the
# arguments after it), in a session of its own (setsid -f returns at once) and
# a scope of its own in the user's systemd: the watcher's unit, which ends with
# its script, would take along all it started
spawn() {
  p=$1
  shift
  systemd-run --user --scope --collect --quiet -- setsid -f env -i "$@" /bin/sh /usr/bin/lwrespawn "$p" ||
    setsid -f env -i "$@" /bin/sh /usr/bin/lwrespawn "$p"
}

# the watcher for PID: a transient unit of the user's systemd (the launcher
# adopts what it leaves behind and never reaps it), else a session of its own
watch() {
  systemd-run --user --collect --quiet --unit="r86-helpers-$1" "$0" after "$1" 9>&- 2>/dev/null ||
    { setsid "$0" after "$1" </dev/null >/dev/null 2>&1 9>&- & }
}

case ${1:-} in
stop) stop; [ -z "${2:-}" ] || watch "$2" ;;
resume) resume ;;
after) tail --pid="$2" -f /dev/null; resume ;;
spawn) shift; spawn "$@" ;;
*) echo "usage: $0 stop [PID] | resume | after PID" >&2; exit 2 ;;
esac
