#!/bin/bash
# Builds lumen and installs it for the current user, into ~/.local/bin.
# On Arch you can install the lumen-icue package from the AUR instead.
set -euo pipefail
cd "$(dirname "$0")"

BIN="$HOME/.local/bin"
UNITS="$HOME/.config/systemd/user"

if [[ -x /usr/bin/lumen-icued ]]; then
  echo "lumen is already installed system-wide (the lumen-icue package). Remove that first, or use it instead." >&2
  exit 1
fi

cargo build --release --locked

# lumen 0.1 installed the daemon as lumend and the TUI as lumen. Stop and remove those so
# the old daemon can't hold the devices alongside the new one.
if [[ -f "$UNITS/lumend.service" ]]; then
  echo "Removing the lumen 0.1 service (lumend)…"
  systemctl --user disable --now lumend.service || true
  rm -f "$UNITS/lumend.service"
fi
rm -f "$BIN/lumend"
# Only remove ~/.local/bin/lumen if it's our old TUI, never someone else's `lumen`.
if [[ -f "$BIN/lumen" ]] && grep -aq "lumen_ipc" "$BIN/lumen"; then
  rm -f "$BIN/lumen"
fi

install -Dm755 target/release/lumen-icued target/release/lumenctl target/release/lumen-tui -t "$BIN/"
install -d "$UNITS"
sed "s#^ExecStart=.*#ExecStart=%h/.local/bin/lumen-icued#" dist/lumen-icued.service > "$UNITS/lumen-icued.service"
install -Dm644 dist/lumen.desktop "$HOME/.local/share/applications/lumen.desktop"

if ! cmp -s dist/70-lumen.rules /etc/udev/rules.d/70-lumen.rules; then
  echo "Installing udev rule (needs root)…"
  sudo install -m644 dist/70-lumen.rules /etc/udev/rules.d/
  sudo udevadm control --reload
  sudo udevadm trigger --subsystem-match=hidraw
fi

systemctl --user daemon-reload
systemctl --user enable lumen-icued.service
systemctl --user restart lumen-icued.service
echo "lumen-icued is running. Open 'Lumen' from the launcher, or run: lumen-tui / lumenctl"
echo "For the Omarchy bar widget: omarchy plugin add https://github.com/pxSean/omarchy-lumen.git --enable"
