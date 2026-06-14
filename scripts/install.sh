#!/usr/bin/env bash
# Maturana Linux installer. Idempotent leaf adapter: the lifecycle logic lives
# in the Rust CLI (`maturana service`, `maturana pipelock`). By default it
# downloads the signed prebuilt `maturana` binary from the latest GitHub Release
# (no Rust/C toolchain needed) and falls back to a source build if no prebuilt
# is available for this arch (or with --from-source).
#
#   curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- --firecracker        # also provision the microVM host
#   curl -fsSL .../install.sh | bash -s -- --from-source        # build locally instead of downloading
#
set -euo pipefail

REPO_URL="${MATURANA_REPO_URL:-https://github.com/ajensenwaud/maturana.git}"
DEST="${MATURANA_DIR:-$HOME/maturana}"
REL_BASE="${MATURANA_RELEASE_BASE:-https://github.com/ajensenwaud/maturana/releases/latest/download}"

WITH_FIRECRACKER=0
FROM_SOURCE=0
# Whether to install skills as Codex /slash commands in ~/.codex/prompts.
# "" = ask (interactive), "1" = yes, "0" = no (keep them in the repo only).
CODEX_PROMPTS="${MATURANA_CODEX_PROMPTS:-}"
for arg in "$@"; do
  case "$arg" in
    --firecracker) WITH_FIRECRACKER=1 ;;
    --from-source) FROM_SOURCE=1 ;;
    --codex-prompts) CODEX_PROMPTS=1 ;;
    --no-codex-prompts) CODEX_PROMPTS=0 ;;
  esac
done

say() { printf '\033[36m[maturana]\033[0m %s\n' "$*"; }
die() { printf '\033[31m[maturana] %s\033[0m\n' "$*" >&2; exit 1; }

# 1. Base tools always needed: git (clone repo assets) + curl + tar.
if ! command -v git >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1; then
  say "installing git + curl (sudo)"
  if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update -qq && sudo apt-get install -y -qq git curl ca-certificates tar
  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y git curl ca-certificates tar
  else
    die "unsupported distro: install git + curl manually, then re-run"
  fi
fi

# 2. Source: clone or update (skills/, AGENTS.md, scripts, examples are needed at runtime).
if [ -d "$DEST/.git" ]; then
  say "updating $DEST"
  git -C "$DEST" pull --ff-only
elif [ -d "$DEST" ]; then
  say "$DEST exists without git metadata; leaving source as-is"
else
  say "cloning into $DEST"
  git clone "$REPO_URL" "$DEST"
fi

mkdir -p "$HOME/.local/bin"
BIN=""

# 3. Prefer the signed prebuilt binary (no toolchain). x86_64 only for now.
if [ "$FROM_SOURCE" = "0" ]; then
  case "$(uname -m)" in
    x86_64) TRIPLE="x86_64-unknown-linux-gnu" ;;
    *)      TRIPLE="" ;;
  esac
  if [ -n "$TRIPLE" ]; then
    asset="maturana-${TRIPLE}.tar.gz"
    tmp="$(mktemp -d)"
    say "downloading prebuilt $asset"
    if curl -fSL "$REL_BASE/$asset" -o "$tmp/$asset" \
       && curl -fSL "$REL_BASE/SHA256SUMS" -o "$tmp/SHA256SUMS"; then
      want="$(grep "$asset" "$tmp/SHA256SUMS" | awk '{print $1}' | head -1)"
      got="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
      if [ -n "$want" ] && [ "$want" = "$got" ]; then
        tar -xzf "$tmp/$asset" -C "$tmp"
        install -m 0755 "$tmp/maturana" "$HOME/.local/bin/maturana"
        BIN="$HOME/.local/bin/maturana"
        say "checksum OK; installed prebuilt binary"
      else
        say "checksum mismatch or missing (want=$want got=$got); falling back to source build"
      fi
    else
      say "no prebuilt release found for $TRIPLE; falling back to source build"
    fi
    rm -rf "$tmp"
  else
    say "no prebuilt for $(uname -m); building from source"
  fi
fi

# 4. Source build fallback (needs a C toolchain + Rust).
if [ -z "$BIN" ]; then
  if ! command -v cc >/dev/null 2>&1; then
    say "installing build tools (sudo)"
    if command -v apt-get >/dev/null 2>&1; then
      sudo apt-get install -y -qq build-essential pkg-config
    elif command -v dnf >/dev/null 2>&1; then
      sudo dnf install -y gcc make pkgconf
    fi
  fi
  if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    say "installing rustup"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  say "building (release)"
  cargo build --release --manifest-path "$DEST/Cargo.toml" -p maturana-cli
  ln -sf "$DEST/target/release/maturana" "$HOME/.local/bin/maturana"
  BIN="$HOME/.local/bin/maturana"
fi

case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) say "add ~/.local/bin to PATH" ;;
esac

# 4b. KVM: agents run in Firecracker microVMs, which need /dev/kvm. Check +
#     enable it on every Linux install so the failure surfaces here (with a fix
#     attempt and clear guidance) instead of later at agent launch. Best-effort:
#     a control-plane-only box without virtualization can still run the CLI/web,
#     so we warn rather than abort. The --firecracker path enables KVM itself
#     (fatally) inside install-firecracker-host.sh, so skip the duplicate here.
if [ "$(uname -s)" = "Linux" ] && [ "$WITH_FIRECRACKER" = "0" ]; then
  if ! bash "$DEST/scripts/kvm-enable.sh"; then
    say "WARNING: KVM is not enabled — agents need it to launch. Fix the cause"
    say "above (firmware virtualization / nested virt), then re-run"
    say "'$DEST/scripts/kvm-enable.sh'. The CLI and web cockpit still installed."
  fi
fi

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
"$BIN" pipelock init >/dev/null 2>&1 || true

# Install Maturana's skills as native Codex skills (~/.agents/skills, discovered
# via /skills or $name) vs kept in the repo only (Codex still loads them via
# AGENTS.md). Ask unless told via flag/env.
if [ -z "$CODEX_PROMPTS" ]; then
  if [ -r /dev/tty ]; then
    printf '\033[36m[maturana]\033[0m Install Maturana skills as Codex skills (~/.agents/skills)? [Y/n] ' > /dev/tty
    read -r _ans < /dev/tty || _ans=""
    case "$_ans" in n|N|no|NO) CODEX_PROMPTS=0 ;; *) CODEX_PROMPTS=1 ;; esac
  else
    CODEX_PROMPTS=1   # non-interactive default: install them
  fi
fi
if [ "$CODEX_PROMPTS" = "1" ]; then
  "$BIN" skill codex-prompts "$DEST/skills" >/dev/null 2>&1 \
    && say "skills installed as Codex skills (use /skills or \$<name> in Codex)" \
    || say "could not install Codex skills (they still load via AGENTS.md)"
else
  say "skills kept in the repo (Codex loads them on demand via AGENTS.md)"
fi

say "registering services (maturana up + maturana web)"
"$BIN" service install up web
# Firecracker hosts also get the boot-time fleet relauncher (zero-touch reboot
# recovery): a systemd oneshot that recreates the TAP + relaunches the microVMs
# from the baked rootfs after a reboot, with no interactive login.
if [ "$WITH_FIRECRACKER" = "1" ] && [ "$(uname -s)" = "Linux" ]; then
  say "registering fleet boot service (maturana fleet)"
  "$BIN" service install fleet
fi

# 7. Orientation: Codex-native. Harness credential pre-check + first-agent steps.
harness_status() {
  # $1 cli, $2 auth path, $3 login hint, $4 install hint
  if command -v "$1" >/dev/null 2>&1 && [ -f "$2" ]; then
    echo "ready"
  elif command -v "$1" >/dev/null 2>&1; then
    echo "installed, NOT logged in -> run: $3"
  else
    echo "missing -> install: $4  then: $3"
  fi
}
codex_status="$(harness_status codex "$HOME/.codex/auth.json" 'codex login' 'npm install -g @openai/codex')"
claude_status="$(harness_status claude "$HOME/.claude/.credentials.json" 'claude (then /login)' 'npm install -g @anthropic-ai/claude-code')"
token="$(head -1 "$DEST/.maturana/web/token" 2>/dev/null || echo '(run: maturana web token)')"

echo
echo "==================== Maturana ready ===================="
echo "A Codex-native agent framework. Build agents from Codex,"
echo "which is oriented by this repo's AGENTS.md + skills/."
echo
echo "1) Authenticate a harness (agents need at least one):"
echo "     codex  : $codex_status"
echo "     claude : $claude_status"
echo
echo "2) Build your first agent:"
echo "     cd $DEST"
echo "     codex"
echo "   then ask Codex: \"create and launch a new agent\","
echo "   or invoke a skill directly: type /skills, or \$maturana-agent-create"
echo "   (all 31 skills are installed as Codex skills under ~/.agents/skills)."
echo
echo "Web cockpit:  http://$(hostname):47836"
echo "     token:  $token"
if [ "$WITH_FIRECRACKER" = "1" ]; then
  echo
  echo "Firecracker microVM host ready; isolated agents relaunch after reboot."
fi
# KVM group membership added during install does NOT apply to this shell or to
# already-running user services. If /dev/kvm exists but isn't accessible yet,
# agents can't launch until a new login session picks up the kvm group. Make
# that the loud, last thing the user sees so a one-shot install doesn't appear
# done while the very next launch would fail.
if [ "$(uname -s)" = "Linux" ] && [ -e /dev/kvm ] && { [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; }; then
  echo
  echo "!! ACTION NEEDED: you were added to the 'kvm' group, but it isn't active"
  echo "   in this session. Log out and back in (or reboot) BEFORE launching"
  echo "   agents, so the runners can open /dev/kvm. Quick check afterwards:"
  echo "     [ -r /dev/kvm ] && [ -w /dev/kvm ] && echo 'kvm OK'"
fi
echo
echo "Help:  maturana --help        Docs:  $DEST/docs"
echo "========================================================"
