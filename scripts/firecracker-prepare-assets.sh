#!/usr/bin/env bash
set -euo pipefail

output_dir="${1:-.maturana/images/firecracker}"
ssh_key_path="${2:-$output_dir/maturana-firecracker.id_rsa}"
auth_source="${3:-}"

release="${MATURANA_UBUNTU_RELEASE:-noble}"
case "$(uname -m)" in
  x86_64) ubuntu_arch="${MATURANA_UBUNTU_ARCH:-amd64}" ;;
  aarch64|arm64) ubuntu_arch="${MATURANA_UBUNTU_ARCH:-arm64}" ;;
  *) ubuntu_arch="${MATURANA_UBUNTU_ARCH:-amd64}" ;;
esac

guest_ip="${MATURANA_FIRECRACKER_GUEST_IP:-172.30.0.2}"
host_ip="${MATURANA_FIRECRACKER_HOST_IP:-172.30.0.1}"
guest_mac="${MATURANA_FIRECRACKER_GUEST_MAC:-aa:fc:00:00:00:01}"
tap_name="${MATURANA_FIRECRACKER_TAP_NAME:-}"
agent_id="${MATURANA_AGENT_ID:-firecracker-demo}"
asset_manifest_path="${MATURANA_FIRECRACKER_ASSET_MANIFEST_PATH:-$output_dir/asset-manifest.json}"
proxy_env_path="${MATURANA_PROXY_ENV_PATH:-}"
proxy_ca_cert_path="${MATURANA_PROXY_CA_CERT_PATH:-}"
sessiond_env_path="${MATURANA_SESSIOND_ENV_PATH:-}"
run_agent_path="${MATURANA_RUN_AGENT_PATH:-}"
agent_service_path="${MATURANA_AGENT_SERVICE_PATH:-}"
harness_install_path="${MATURANA_HARNESS_INSTALL_PATH:-}"
harness_install_service_path="${MATURANA_HARNESS_INSTALL_SERVICE_PATH:-}"
firecracker_bootstrap_path="${MATURANA_FIRECRACKER_BOOTSTRAP_PATH:-}"
netplan_path="${MATURANA_NETPLAN_PATH:-}"
cloud_cfg_path="${MATURANA_CLOUD_CFG_PATH:-}"

image_name="${release}-server-cloudimg-${ubuntu_arch}.img"
image_url="${MATURANA_UBUNTU_IMAGE_URL:-https://cloud-images.ubuntu.com/$release/current/$image_name}"
sha_url="${MATURANA_UBUNTU_SHA256SUMS_URL:-https://cloud-images.ubuntu.com/$release/current/SHA256SUMS}"

kernel_out="$output_dir/vmlinux.bin"
rootfs_out="$output_dir/ubuntu-rootfs.ext4"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    echo "install on Ubuntu with: sudo apt-get install -y qemu-utils libguestfs-tools" >&2
    exit 1
  fi
}

require_file() {
  local path="$1"
  local name="$2"
  if [[ -z "$path" || ! -f "$path" ]]; then
    echo "$name must point to a Rust-rendered file" >&2
    exit 1
  fi
}

need curl
need sha256sum
need qemu-img
need virt-resize
need virt-filesystems
need virt-copy-in
need virt-copy-out
need virt-customize
need guestfish
need tar
need mkfs.ext4
need truncate
need ssh-keygen

# libguestfs/supermin builds its appliance from the host kernel and picks the
# NEWEST /boot/vmlinuz-* (sort -V). Ubuntu ships those mode 0600, so when this
# script runs libguestfs as a non-root user every virt-* call dies with
# "supermin exited with error status 1" — and the failure only surfaces minutes
# later as a cryptic "could not find vmlinuz-* in the Ubuntu image" after the
# (empty) virt-copy-out. Catch it up front: auto-fix with passwordless sudo if
# available, otherwise fail FAST with the exact remedy.
ensure_guest_build_kernel_readable() {
  local newest
  newest="$(ls -1 /boot/vmlinuz-* 2>/dev/null | sort -V | tail -n 1)"
  [[ -n "$newest" && -e "$newest" ]] || return 0   # no host kernel here — leave it to libguestfs
  [[ -r "$newest" ]] && return 0                    # already readable
  if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
    echo "making /boot/vmlinuz-* readable for libguestfs (sudo)" >&2
    sudo chmod 0644 /boot/vmlinuz-* 2>/dev/null || true
  fi
  if [[ ! -r "$newest" ]]; then
    echo "libguestfs cannot read the host kernel $newest." >&2
    echo "Ubuntu ships /boot/vmlinuz-* mode 0600; the firecracker image build runs" >&2
    echo "libguestfs as your user and needs it readable. Fix once and re-run:" >&2
    echo "  sudo chmod 0644 /boot/vmlinuz-*" >&2
    echo "(scripts/install-firecracker-host.sh also makes this durable across kernel upgrades.)" >&2
    exit 1
  fi
}
ensure_guest_build_kernel_readable

require_file "$sessiond_env_path" "MATURANA_SESSIOND_ENV_PATH"
require_file "$run_agent_path" "MATURANA_RUN_AGENT_PATH"
require_file "$agent_service_path" "MATURANA_AGENT_SERVICE_PATH"
require_file "$harness_install_path" "MATURANA_HARNESS_INSTALL_PATH"
require_file "$firecracker_bootstrap_path" "MATURANA_FIRECRACKER_BOOTSTRAP_PATH"
require_file "$netplan_path" "MATURANA_NETPLAN_PATH"
require_file "$cloud_cfg_path" "MATURANA_CLOUD_CFG_PATH"
if [[ -z "$auth_source" ]]; then
  echo "auth source must be supplied by Rust; no default harness auth path is allowed" >&2
  exit 1
fi

find_extract_vmlinux() {
  if [[ -n "${MATURANA_EXTRACT_VMLINUX:-}" ]]; then
    echo "$MATURANA_EXTRACT_VMLINUX"
    return
  fi
  if command -v extract-vmlinux >/dev/null 2>&1; then
    command -v extract-vmlinux
    return
  fi
  find /usr/src /usr/lib -path '*/scripts/extract-vmlinux' -type f 2>/dev/null | sort -V | tail -n 1
}

extract_vmlinux="$(find_extract_vmlinux)"
if [[ -z "$extract_vmlinux" || ! -f "$extract_vmlinux" ]]; then
  echo "missing required Linux helper: extract-vmlinux" >&2
  echo "install Linux headers or set MATURANA_EXTRACT_VMLINUX=/path/to/extract-vmlinux" >&2
  exit 1
fi

detect_root_partition() {
  virt-filesystems -a "$1" --filesystems --long |
    awk '$4 == "cloudimg-rootfs" { print $1; found=1; exit } END { if (!found) exit 1 }'
}

mkdir -p "$output_dir"
work_base="${MATURANA_FIRECRACKER_WORK_BASE:-$output_dir/.work}"
mkdir -p "$work_base"
work_dir="$(mktemp -d "$work_base/work.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT

image_path="$output_dir/$image_name"
sha_path="$output_dir/SHA256SUMS"
work_img="$work_dir/ubuntu-cloudimg.qcow2"
resized_img="$work_dir/ubuntu-resized.qcow2"
disk_size="${MATURANA_FIRECRACKER_DISK_SIZE:-8G}"

if [[ ! -f "$image_path" ]]; then
  echo "Downloading Ubuntu cloud image: $image_url"
  curl -fsSL "$image_url" -o "$image_path"
else
  echo "Using existing Ubuntu cloud image: $image_path"
fi

echo "Downloading checksum file: $sha_url"
curl -fsSL "$sha_url" -o "$sha_path"

expected="$(grep -E "[ *]$image_name$" "$sha_path" | awk '{print $1}' | head -n 1)"
if [[ -z "$expected" ]]; then
  echo "no checksum entry for $image_name in $sha_path" >&2
  exit 1
fi

actual="$(sha256sum "$image_path" | awk '{print $1}')"
if [[ "$actual" != "$expected" ]]; then
  echo "checksum mismatch for $image_path" >&2
  echo "expected: $expected" >&2
  echo "actual:   $actual" >&2
  exit 1
fi
echo "Checksum OK."

if [[ ! -f "$ssh_key_path" ]]; then
  ssh-keygen -t ed25519 -f "$ssh_key_path" -N ""
fi

# Bake a known SSH *host* key into the rootfs so the host can verify the guest's
# identity instead of trusting whatever server answers (StrictHostKeyChecking).
# Its public key is published in the asset manifest and pinned per agent.
host_key_path="${MATURANA_FIRECRACKER_HOST_KEY_PATH:-$output_dir/maturana-firecracker-host.ed25519}"
if [[ ! -f "$host_key_path" ]]; then
  ssh-keygen -t ed25519 -f "$host_key_path" -N "" -C "maturana-host"
fi
host_pub_line="$(tr -d '\n' < "$host_key_path.pub")"

echo "Expanding Ubuntu image to $disk_size..."
source_root_partition="${MATURANA_UBUNTU_SOURCE_ROOT_PARTITION:-$(detect_root_partition "$image_path")}"
qemu-img create -q -f qcow2 "$resized_img" "$disk_size"
virt-resize --expand "$source_root_partition" "$image_path" "$resized_img" >/dev/null
work_img="$resized_img"
root_partition="${MATURANA_UBUNTU_ROOT_PARTITION:-$(detect_root_partition "$work_img")}"

cp "$netplan_path" "$work_dir/50-maturana-firecracker.yaml"
cp "$cloud_cfg_path" "$work_dir/99-disable-network-config.cfg"

mkdir -p "$work_dir/agent"
for name in MATURANA.md AGENTS.md SOUL.md; do
  if [[ -f ".maturana/agents/$agent_id/$name" ]]; then
    cp ".maturana/agents/$agent_id/$name" "$work_dir/agent/$name"
  elif [[ -f "$name" ]]; then
    cp "$name" "$work_dir/agent/$name"
  fi
done

if [[ -n "$proxy_env_path" ]]; then
  require_file "$proxy_env_path" "MATURANA_PROXY_ENV_PATH"
  cp "$proxy_env_path" "$work_dir/agent/proxy.env"

  if grep -q '^MATURANA_PROXY_HTTPS=1$' "$proxy_env_path"; then
    if [[ -z "$proxy_ca_cert_path" || ! -f "$proxy_ca_cert_path" ]]; then
      echo "MATURANA_PROXY_HTTPS=1 requires MATURANA_PROXY_CA_CERT_PATH to point to the Maturana pipelock CA cert" >&2
      exit 1
    fi
    cp "$proxy_ca_cert_path" "$work_dir/maturana-pipelock-ca.crt"
  fi
fi

cp "$sessiond_env_path" "$work_dir/sessiond.env"
cp "$run_agent_path" "$work_dir/run-agent.sh"
cp "$agent_service_path" "$work_dir/maturana-agent.service"
cp "$harness_install_path" "$work_dir/install-harness.sh"
cp "$harness_install_service_path" "$work_dir/maturana-harness-install.service"
cp "$firecracker_bootstrap_path" "$work_dir/firecracker-bootstrap.sh"
cp "$host_key_path" "$work_dir/ssh_host_ed25519_key"
cp "$host_key_path.pub" "$work_dir/ssh_host_ed25519_key.pub"

# Extract the guest kernel BEFORE customizing. The Ubuntu cloud image keeps /boot
# on a SEPARATE partition (label BOOT), and the bootstrap below strips /boot from
# the image's fstab (the firecracker guest has no separate /boot). virt-copy-out
# mounts via the guest fstab, so extracting AFTER the strip yields an empty /boot
# and the misleading "could not find vmlinuz-*". Do it here, while fstab still
# mounts the /boot partition.
reuse_kernel="${MATURANA_REUSE_KERNEL_IMAGE:-}"
if [[ -z "$reuse_kernel" && -f ".maturana/images/firecracker/vmlinux.bin" && "$kernel_out" != ".maturana/images/firecracker/vmlinux.bin" ]]; then
  reuse_kernel=".maturana/images/firecracker/vmlinux.bin"
fi
if [[ -n "$reuse_kernel" ]]; then
  echo "Reusing Firecracker kernel: $reuse_kernel"
  cp "$reuse_kernel" "$kernel_out"
else
  rm -rf "$work_dir/boot"
  virt-copy-out -a "$work_img" /boot "$work_dir"
  kernel_candidate="$(find "$work_dir/boot" -maxdepth 1 -type f -name 'vmlinuz-*' | sort -V | tail -n 1)"
  if [[ -z "$kernel_candidate" ]]; then
    echo "could not find vmlinuz-* in the Ubuntu image" >&2
    exit 1
  fi
  "$extract_vmlinux" "$kernel_candidate" > "$kernel_out"
fi
if ! file "$kernel_out" | grep -q 'ELF'; then
  echo "failed to extract an ELF vmlinux from ${kernel_candidate:-$reuse_kernel}" >&2
  file "$kernel_out" >&2 || true
  exit 1
fi

echo "Customizing Ubuntu image offline..."
virt-copy-in -a "$work_img" "$work_dir/firecracker-bootstrap.sh" /tmp
virt-customize -a "$work_img" \
  --run-command 'chmod 0755 /tmp/firecracker-bootstrap.sh' \
  --run-command '/tmp/firecracker-bootstrap.sh' \
  --ssh-inject "ubuntu:file:$ssh_key_path.pub"

virt-copy-in -a "$work_img" "$work_dir/50-maturana-firecracker.yaml" /etc/netplan
virt-copy-in -a "$work_img" "$work_dir/99-disable-network-config.cfg" /etc/cloud/cloud.cfg.d
virt-copy-in -a "$work_img" "$work_dir/agent" /
virt-copy-in -a "$work_img" "$work_dir/sessiond.env" /agent
virt-copy-in -a "$work_img" "$work_dir/run-agent.sh" /opt/maturana/bin
virt-copy-in -a "$work_img" "$work_dir/maturana-agent.service" /etc/systemd/system
# Install the harness via a first-boot one-shot in the guest (over its own
# network) rather than in this offline build appliance, whose network is
# unreliable on some hosts and can hang npm.
virt-copy-in -a "$work_img" "$work_dir/install-harness.sh" /opt/maturana/bin
virt-copy-in -a "$work_img" "$work_dir/maturana-harness-install.service" /etc/systemd/system
# After firecracker-bootstrap.sh ran `ssh-keygen -A` (which created a fresh
# ed25519 host key), overwrite it with the baked one so it matches the manifest.
virt-copy-in -a "$work_img" "$work_dir/ssh_host_ed25519_key" "$work_dir/ssh_host_ed25519_key.pub" /etc/ssh

if [[ -f "$work_dir/maturana-pipelock-ca.crt" ]]; then
  virt-copy-in -a "$work_img" "$work_dir/maturana-pipelock-ca.crt" /usr/local/share/ca-certificates
fi

if [[ -d "$auth_source" ]]; then
  mkdir -p "$work_dir/.codex"
  cp -a "$auth_source"/. "$work_dir/.codex"/
  virt-copy-in -a "$work_img" "$work_dir/.codex" /home/ubuntu
fi

# (Guest kernel was already extracted above, before the bootstrap stripped /boot
# from the image fstab — see the extraction block ahead of "Customizing ...".)

virt-customize -a "$work_img" \
  --run-command 'chmod 0600 /etc/netplan/50-maturana-firecracker.yaml' \
  --run-command 'chmod 0600 /etc/ssh/ssh_host_ed25519_key' \
  --run-command 'chmod 0644 /etc/ssh/ssh_host_ed25519_key.pub' \
  --run-command 'chown root:root /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub' \
  --run-command 'chmod 0600 /agent/sessiond.env' \
  --run-command 'chmod 0755 /opt/maturana/bin/run-agent.sh' \
  --run-command 'chmod 0755 /opt/maturana/bin/install-harness.sh' \
  --run-command 'systemctl enable maturana-harness-install.service || true' \
  --run-command 'if [ -f /usr/local/share/ca-certificates/maturana-pipelock-ca.crt ]; then update-ca-certificates; fi' \
  --run-command 'chown -R ubuntu:ubuntu /agent /workspace /memory /wiki /var/log/maturana /home/ubuntu/.codex 2>/dev/null || true' \
  --run-command 'chmod -R go-rwx /home/ubuntu/.codex 2>/dev/null || true' \
  --run-command 'systemctl enable maturana-agent.service || true' \
  --run-command 'apt-get clean || true' \
  --run-command 'rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/* /root/.npm /home/ubuntu/.npm /tmp/* || true'

echo "Exporting root filesystem from $root_partition..."
mkdir -p "$work_dir/root"
guestfish --ro -a "$work_img" -m "$root_partition":/ tar-out / "$work_dir/root.tar"
tar -C "$work_dir/root" -xf "$work_dir/root.tar"

echo "Creating ext4 rootfs: $rootfs_out"
rm -f "$rootfs_out"
truncate -s "$disk_size" "$rootfs_out"
mkfs.ext4 -q -d "$work_dir/root" -F "$rootfs_out"

echo "Prepared Firecracker assets:"
echo "kernel: $kernel_out"
echo "rootfs: $rootfs_out"
echo "ssh_key: $ssh_key_path"
echo "ssh: ssh -i \"$ssh_key_path\" ubuntu@$guest_ip"

kernel_sha256="$(sha256sum "$kernel_out" | awk '{print $1}')"
rootfs_sha256="$(sha256sum "$rootfs_out" | awk '{print $1}')"
kernel_bytes="$(wc -c < "$kernel_out" | tr -d ' ')"
rootfs_bytes="$(wc -c < "$rootfs_out" | tr -d ' ')"
mkdir -p "$(dirname "$asset_manifest_path")"
cat > "$asset_manifest_path" <<EOF
{
  "agent_id": "$agent_id",
  "kernel": "$kernel_out",
  "rootfs": "$rootfs_out",
  "ssh_key": "$ssh_key_path",
  "ssh_host_ed25519_pub": "$host_pub_line",
  "guest_ip": "$guest_ip",
  "host_ip": "$host_ip",
  "guest_mac": "$guest_mac",
  "tap_name": "$tap_name",
  "kernel_sha256": "$kernel_sha256",
  "rootfs_sha256": "$rootfs_sha256",
  "kernel_bytes": $kernel_bytes,
  "rootfs_bytes": $rootfs_bytes
}
EOF
echo "manifest: $asset_manifest_path"

# Make the whole output dir owned by the invoking user. The libguestfs /
# virt-customize steps run via sudo and leave root-owned files here — including
# maturana-firecracker.id_rsa, which the (non-root) agent launcher must read to
# SSH into the guest. Without this a fresh host fails with
# `Load key "...id_rsa": Permission denied` -> guest publickey denied.
if [[ "$(id -u)" -eq 0 && -n "${SUDO_UID:-}" && -n "${SUDO_GID:-}" ]]; then
  # Script itself runs as root under sudo: hand ownership to the real user.
  chown -R "$SUDO_UID:$SUDO_GID" "$output_dir"
elif [[ "$(id -u)" -ne 0 ]] && command -v sudo >/dev/null 2>&1; then
  # Script runs as a normal user that used sudo internally for libguestfs:
  # reclaim any root-owned artifacts back to the current user.
  sudo chown -R "$(id -u):$(id -g)" "$output_dir" 2>/dev/null || true
fi
