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

# Omarchy bar widget (hot-reloads; enable once with: omarchy plugin enable sfinkel.lumen --section right)
PLUGIN_DIR="$HOME/.config/omarchy/plugins/sfinkel.lumen"
plugin_changed=false
cmp -s dist/omarchy-plugin/Panel.qml "$PLUGIN_DIR/Panel.qml" || plugin_changed=true
install -Dm644 dist/omarchy-plugin/manifest.json dist/omarchy-plugin/Panel.qml -t "$PLUGIN_DIR/"

systemctl --user daemon-reload
systemctl --user enable lumend.service
systemctl --user restart lumend.service
if $plugin_changed; then
  # Hot reload swaps the bar button but keeps the old popup window alive.
  echo "The Lumen bar widget changed: run 'omarchy restart shell' to reload its panel."
fi
echo "lumend is running. Open 'Lumen' from the launcher, or run: lumen / lumenctl"
