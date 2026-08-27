#!/usr/bin/env bash
# Build and install the Maschine MK3 driver for the current user.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

bindir="${HOME}/.local/bin"
rules=/etc/udev/rules.d/98-maschine-mk3.rules
unitdir="${HOME}/.config/systemd/user"

echo "==> building"
cargo build --release

echo "==> installing binaries into ${bindir}"
mkdir -p "$bindir"
install -m755 target/release/mk3d "$bindir/"
install -m755 target/release/mk3-learn "$bindir/"
install -m755 target/release/mk3-gui "$bindir/"

echo "==> installing desktop entry"
install -d "${HOME}/.local/share/applications"
install -m644 desktop/maschine-mk3.desktop "${HOME}/.local/share/applications/"

if [ ! -f "$rules" ] || ! cmp -s udev/98-maschine-mk3.rules "$rules"; then
  echo "==> installing udev rules (needs sudo)"
  sudo install -m644 udev/98-maschine-mk3.rules "$rules"
  sudo udevadm control --reload
  sudo udevadm trigger --subsystem-match=usb --subsystem-match=hidraw
  echo "    unplug and replug the Maschine so the new permissions take effect"
fi

echo "==> installing presets and the device profile"
install -d "${HOME}/.config/maschine-mk3/presets" "${HOME}/.config/maschine-mk3/devices"
# Only copy a preset that is not already there: an edited one must not be
# clobbered by an upgrade.
for f in presets/*.toml; do
  [ -e "${HOME}/.config/maschine-mk3/presets/$(basename "$f")" ] || install -m644 "$f" "${HOME}/.config/maschine-mk3/presets/"
done
install -m644 devices/maschine-mk3.toml "${HOME}/.config/maschine-mk3/devices/"

echo "==> installing systemd user unit"
mkdir -p "$unitdir"
install -m644 systemd/maschine-mk3d.service "$unitdir/"
systemctl --user daemon-reload

cat <<MSG

Installed.

  mk3-gui                   the configuration window (starts the driver too)
  mk3d                      run the driver in the foreground
  mk3d --list-presets       ready-made settings to start from
  mk3-learn buttons         map your unit's buttons
  systemctl --user enable --now maschine-mk3d
                            run it automatically

Config: ${HOME}/.config/maschine-mk3/config.toml
MSG
