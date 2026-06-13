#!/usr/bin/env bash
# Maturana Linux uninstaller. Removes the systemd user units, the runtime plane,
# Firecracker microVMs + TAPs, and the linked binary. By default it KEEPS your
# data (the repo + .maturana, which holds credentials/agents); pass --purge to
# remove everything.
#
#   curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/uninstall.sh | bash
#   curl -fsSL .../scripts/uninstall.sh | bash -s -- --purge
#
set -euo pipefail

DEST="${MATURANA_DIR:-$HOME/maturana}"
PURGE=0
for arg in "$@"; do
  case "$arg" in --purge) PURGE=1 ;; esac
done

say() { printf '\033[36m[maturana]\033[0m %s\n' "$*"; }

# 1. Stop + disable the systemd user units, then remove the unit files.
systemctl --user disable --now maturana-up maturana-web maturana-fleet 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/maturana-up.service" \
      "$HOME/.config/systemd/user/maturana-web.service" \
      "$HOME/.config/systemd/user/maturana-fleet.service"
systemctl --user daemon-reload 2>/dev/null || true
say "removed systemd user units"

# 2. Kill any stray plane processes. Match the exact process name (not -f, which
#    would also match this script's own command line) to avoid self-termination.
pkill -x maturana 2>/dev/null || true
sleep 1

# 3. Stop Firecracker microVMs + delete their ephemeral TAP devices.
for p in $(pgrep -x firecracker 2>/dev/null || true); do sudo kill "$p" 2>/dev/null || true; done
sleep 1
for p in $(pgrep -x firecracker 2>/dev/null || true); do sudo kill -9 "$p" 2>/dev/null || true; done
for tap in tap-mat-codex tap-mat-open tap-mat-claude tap-maturana0; do
  sudo ip link delete "$tap" 2>/dev/null || true
done
say "stopped microVMs + removed TAPs"

# 4. Remove the linked binary.
rm -f "$HOME/.local/bin/maturana"

# 5. Purge data + repo, or keep it.
if [ "$PURGE" = "1" ]; then
  rm -rf "$DEST"
  say "purged $DEST (repo + .maturana, including credentials)"
else
  say "kept $DEST (repo + .maturana data). Re-run with --purge to remove it too."
fi

say "Maturana uninstalled."
echo "  (systemd linger left enabled; disable with: loginctl disable-linger \$USER)"
