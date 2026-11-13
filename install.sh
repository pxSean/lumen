#!/bin/bash
# Builds lumen and installs it for the current user.
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release
install -Dm755 target/release/lumend target/release/lumenctl target/release/lumen -t "$HOME/.local/bin/"
install -Dm644 dist/lumend.service "$HOME/.config/systemd/user/lumend.service"
install -Dm644 dist/lumen.desktop "$HOME/.local/share/applications/lumen.desktop"

if ! cmp -s dist/70-lumen.rules /etc/udev/rules.d/70-lumen.rules; then
  echo "Installing udev rule (needs root)…"
  sudo install -m644 dist/70-lumen.rules /etc/udev/rules.d/
  sudo udevadm control --reload
  sudo udevadm trigger --subsystem-match=hidraw
fi

systemctl --user daemon-reload
systemctl --user enable lumend.service
systemctl --user restart lumend.service
echo "lumend is running. Open 'Lumen' from the launcher, or run: lumen / lumenctl"
echo "For the Omarchy bar widget: omarchy plugin add https://github.com/pxSean/omarchy-lumen.git --enable"
