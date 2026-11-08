#!/bin/sh
# Installs the app beside its firmware and complete DOS kit, plus the launcher.
# APPLaunch starts apps as the desktop user without a terminal; the wrapper
# keeps VMs, downloads and the log in that user's ~/r86 (the package's own
# directory is root-owned and replaced on upgrade).
set -eu
root=$(cd "$(dirname "$0")/../../.." && pwd)
app=$STAGE$APP_INSTALL_DIR
install -D -m 0755 "$root/target/aarch64-unknown-linux-musl/release/r86-handheld" "$app/r86-handheld"
"$root/ports/share.sh" "$app"

cat >"$STAGE$INSTALL_PREFIX/bin/$PKG_NAME" <<WRAPPER
#!/bin/sh
data=\$HOME/r86
mkdir -p "\$data"
exec $APP_INSTALL_DIR/r86-handheld --library "\$data/VMs" --cache "\$data/cache" "\$@" \\
    >"\$data/r86.log" 2>&1
WRAPPER
chmod 0755 "$STAGE$INSTALL_PREFIX/bin/$PKG_NAME"
