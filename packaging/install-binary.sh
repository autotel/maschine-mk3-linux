#!/usr/bin/env bash
# Install a prebuilt Maschine MK3 driver for the current user.
#
# The counterpart to ../install.sh, for the release tarball: same steps, minus
# the build, because the binaries are already here.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

bindir="${HOME}/.local/bin"
rules=/etc/udev/rules.d/98-maschine-mk3.rules
unitdir="${HOME}/.config/systemd/user"

echo "==> installing binaries into ${bindir}"
mkdir -p "$bindir"
install -m755 bin/mk3d "$bindir/"
install -m755 bin/mk3-learn "$bindir/"
install -m755 bin/mk3-gui "$bindir/"

echo "==> installing desktop entry"
install -d "${HOME}/.local/share/applications"
install -m644 desktop/maschine-mk3.desktop "${HOME}/.local/share/applications/"

if [ ! -f "$rules" ] || ! cmp -s udev/98-maschine-mk3.rules "$rules"; then
  echo "==> installing udev rules (needs sudo)"
  sudo install -m644 udev/98-maschine-mk3.rules "$rules"
  sudo udevadm control --reload
  sudo udevadm trigger --subsystem-match=usb --subsystem-match=hidraw
  echo "    unplug and replug the Maschine so the new permissions take effect"
else
  echo "==> udev rules already current"
fi

echo "==> installing systemd user unit into ${unitdir}"
install -d "$unitdir"
install -m644 systemd/maschine-mk3d.service "$unitdir/"
systemctl --user daemon-reload

case ":${PATH}:" in
  *":${bindir}:"*) ;;
  *) echo "note: ${bindir} is not on your PATH" ;;
esac

cat <<'EOF'

installed. next:

  mk3d --diagnose     check the device, permissions and routing
  mk3d                run in the foreground
  mk3-gui             configure it in a window

  systemctl --user enable --now maschine-mk3d    # or run it as a service

if something does not work, docs/latency.md and the README's troubleshooting
section cover the cases that look like bugs and are not.
EOF
