#!/usr/bin/env bash
# Maturana Linux installer. Idempotent leaf adapter: the lifecycle logic lives
# in the Rust CLI (`maturana service`, `maturana pipelock`); this script only
# bootstraps the toolchain, builds, and hands over.
#
#   curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/install.sh | bash
#
set -euo pipefail

REPO_URL="${MATURANA_REPO_URL:-https://github.com/ajensenwaud/maturana.git}"
DEST="${MATURANA_DIR:-$HOME/maturana}"

# --firecracker also provisions the Linux microVM agent-host substrate
# (firecracker binary, KVM, libguestfs/qemu, NAT) via install-firecracker-host.sh.
WITH_FIRECRACKER=0
for arg in "$@"; do
  case "$arg" in
    --firecracker) WITH_FIRECRACKER=1 ;;
  esac
done

say() { printf '\033[36m[maturana]\033[0m %s\n' "$*"; }

# 1. Base dependencies.
if ! command -v git >/dev/null 2>&1 || ! command -v cc >/dev/null 2>&1; then
  say "installing git + build tools (sudo)"
  if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update -qq && sudo apt-get install -y -qq git build-essential pkg-config curl
  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y git gcc make pkgconf curl
  else
    say "unsupported distro: install git + a C toolchain manually, then re-run"; exit 1
  fi
fi

# 2. Rust toolchain.
if ! command -v cargo >/dev/null 2>&1; then
  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
fi
if ! command -v cargo >/dev/null 2>&1; then
  say "installing rustup"
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi

# 3. Source: clone or update.
if [ -d "$DEST/.git" ]; then
  say "updating $DEST"
  git -C "$DEST" pull --ff-only
elif [ -d "$DEST" ]; then
  say "$DEST exists without git metadata; leaving source as-is"
else
  say "cloning into $DEST"
  git clone "$REPO_URL" "$DEST"
fi

# 4. Build + link.
say "building (release)"
cargo build --release --manifest-path "$DEST/Cargo.toml" -p maturana-cli
mkdir -p "$HOME/.local/bin"
ln -sf "$DEST/target/release/maturana" "$HOME/.local/bin/maturana"
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) say "add ~/.local/bin to PATH" ;;
esac

# 5. Optional: provision the Linux Firecracker agent-host substrate.
if [ "$WITH_FIRECRACKER" = "1" ]; then
  if [ "$(uname -s)" = "Linux" ]; then
    say "provisioning Firecracker agent host"
    bash "$DEST/scripts/install-firecracker-host.sh"
  else
    say "--firecracker ignored: not a Linux host"
  fi
fi

# 6. Initialize + register services (Rust owns the logic).
cd "$DEST"
"$DEST/target/release/maturana" pipelock init >/dev/null 2>&1 || true
say "registering services (maturana up + maturana web)"
"$DEST/target/release/maturana" service install up web
# Firecracker hosts also get the boot-time fleet relauncher (zero-touch reboot
# recovery): a systemd oneshot that recreates the TAP + relaunches the microVMs
# from the baked rootfs after a reboot, with no interactive login.
if [ "$WITH_FIRECRACKER" = "1" ] && [ "$(uname -s)" = "Linux" ]; then
  say "registering fleet boot service (maturana fleet)"
  "$DEST/target/release/maturana" service install fleet
fi

# 7. Orientation: both control surfaces are equals.
say "install complete"
echo
echo "  Two ways to drive Maturana (pick either, or both):"
echo "    1. Codex CLI control plane:  cd $DEST && codex"
echo "       (AGENTS.md + skills/ are the contract that orients it)"
echo "    2. Web cockpit:              http://$(hostname):47836"
echo "       token: $DEST/.maturana/web/token"
echo
if ! command -v codex >/dev/null 2>&1; then
  echo "  note: codex CLI not found - install with: npm install -g @openai/codex"
fi
if [ "$WITH_FIRECRACKER" = "1" ]; then
  echo "    3. Launch isolated agents:   maturana repair firecracker-harnesses"
  echo "       (stage creds under .maturana/host-auth/<harness>/ first)"
  echo "       microVMs relaunch automatically after a reboot (fleet service)"
fi
echo "  boot-time start: linger enabled automatically (zero-touch reboot recovery)"
