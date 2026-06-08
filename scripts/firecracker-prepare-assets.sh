#!/usr/bin/env bash
set -euo pipefail

output_dir="${1:-.maturana/images/firecracker}"
ssh_key_path="${2:-$output_dir/maturana-firecracker.id_rsa}"
auth_source="${3:-.maturana/host-auth/codex}"

release="${MATURANA_UBUNTU_RELEASE:-noble}"
case "$(uname -m)" in
  x86_64) ubuntu_arch="${MATURANA_UBUNTU_ARCH:-amd64}" ;;
  aarch64|arm64) ubuntu_arch="${MATURANA_UBUNTU_ARCH:-arm64}" ;;
  *) ubuntu_arch="${MATURANA_UBUNTU_ARCH:-amd64}" ;;
esac

guest_ip="${MATURANA_FIRECRACKER_GUEST_IP:-172.30.0.2}"
host_ip="${MATURANA_FIRECRACKER_HOST_IP:-172.30.0.1}"
guest_mac="${MATURANA_FIRECRACKER_GUEST_MAC:-aa:fc:00:00:00:01}"
agent_id="${MATURANA_AGENT_ID:-firecracker-demo}"
harness="${MATURANA_HARNESS:-codex}"
proxy_port="${MATURANA_PROXY_PORT:-}"
proxy_https="${MATURANA_PROXY_HTTPS:-0}"
proxy_ca_cert_path="${MATURANA_PROXY_CA_CERT_PATH:-}"

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

echo "Expanding Ubuntu image to $disk_size..."
source_root_partition="${MATURANA_UBUNTU_SOURCE_ROOT_PARTITION:-$(detect_root_partition "$image_path")}"
qemu-img create -q -f qcow2 "$resized_img" "$disk_size"
virt-resize --expand "$source_root_partition" "$image_path" "$resized_img" >/dev/null
work_img="$resized_img"
root_partition="${MATURANA_UBUNTU_ROOT_PARTITION:-$(detect_root_partition "$work_img")}"

cat > "$work_dir/50-maturana-firecracker.yaml" <<YAML
network:
  version: 2
  ethernets:
    eth0:
      match:
        macaddress: "$guest_mac"
      set-name: eth0
      dhcp4: false
      addresses:
        - $guest_ip/30
      routes:
        - to: default
          via: $host_ip
      nameservers:
        addresses:
          - 1.1.1.1
          - 8.8.8.8
YAML

cat > "$work_dir/99-disable-network-config.cfg" <<'CFG'
network: {config: disabled}
CFG

mkdir -p "$work_dir/agent"
for name in MATURANA.md AGENTS.md SOUL.md; do
  if [[ -f ".maturana/agents/$agent_id/$name" ]]; then
    cp ".maturana/agents/$agent_id/$name" "$work_dir/agent/$name"
  elif [[ -f "$name" ]]; then
    cp "$name" "$work_dir/agent/$name"
  fi
done

cat > "$work_dir/agent/prompt.txt" <<'PROMPT'
Inspect /agent/MATURANA.md and report that the Codex guest harness is ready.
PROMPT

if [[ -n "$proxy_port" && "$proxy_port" != "0" ]]; then
  cat > "$work_dir/agent/proxy.env" <<PROXYENV
MATURANA_USE_HOST_PROXY=1
MATURANA_PROXY_HOST=$host_ip
MATURANA_PROXY_PORT=$proxy_port
MATURANA_PROXY_HTTPS=$proxy_https
NO_PROXY=localhost,127.0.0.1,::1
PROXYENV

  if [[ "$proxy_https" == "1" ]]; then
    if [[ -z "$proxy_ca_cert_path" || ! -f "$proxy_ca_cert_path" ]]; then
      echo "MATURANA_PROXY_HTTPS=1 requires MATURANA_PROXY_CA_CERT_PATH to point to the Maturana pipelock CA cert" >&2
      exit 1
    fi
    cp "$proxy_ca_cert_path" "$work_dir/maturana-pipelock-ca.crt"
  fi
fi

cat > "$work_dir/run-agent.sh" <<'RUNNER'
#!/usr/bin/env bash
set -euo pipefail
export MATURANA_AGENT_ID="${MATURANA_AGENT_ID:-firecracker-demo}"
export MATURANA_HARNESS="${MATURANA_HARNESS:-codex}"
export CODEX_HOME="${CODEX_HOME:-/home/ubuntu/.codex}"

mkdir -p /var/log/maturana /workspace /memory /wiki

if [ -f /agent/proxy.env ]; then
  # shellcheck disable=SC1091
  . /agent/proxy.env
  if [ "${MATURANA_USE_HOST_PROXY:-0}" = "1" ] && [ -n "${MATURANA_PROXY_HOST:-}" ] && [ -n "${MATURANA_PROXY_PORT:-}" ]; then
    export HTTP_PROXY="http://${MATURANA_PROXY_HOST}:${MATURANA_PROXY_PORT}"
    export http_proxy="$HTTP_PROXY"
    if [ "${MATURANA_PROXY_HTTPS:-0}" = "1" ]; then
      export HTTPS_PROXY="$HTTP_PROXY"
      export https_proxy="$HTTP_PROXY"
    fi
    export NO_PROXY="${NO_PROXY:-localhost,127.0.0.1,::1}"
    export no_proxy="$NO_PROXY"
  fi
fi

cd /workspace

echo "Maturana $MATURANA_HARNESS agent $MATURANA_AGENT_ID starting"

if ! command -v codex >/dev/null 2>&1 && command -v npm >/dev/null 2>&1; then
  sudo npm install -g @openai/codex >> /var/log/maturana/harness.out.log 2>> /var/log/maturana/harness.err.log || true
fi

if [ -f /agent/run-command ] && [ ! -f /var/log/maturana/run.done ]; then
  echo "Executing /agent/run-command"
  bash /agent/run-command > /var/log/maturana/harness.out.log 2> /var/log/maturana/harness.err.log
  touch /var/log/maturana/run.done
elif [ -f /agent/prompt.txt ] && [ ! -f /var/log/maturana/run.done ]; then
  echo "Executing $MATURANA_HARNESS prompt from /agent/prompt.txt"
  if command -v codex >/dev/null 2>&1; then
    codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -C /workspace -o /var/log/maturana/last-message.txt "$(cat /agent/prompt.txt)" > /var/log/maturana/harness.out.log 2> /var/log/maturana/harness.err.log
  else
    echo "codex is not installed" | tee /var/log/maturana/last-message.txt
  fi
  touch /var/log/maturana/run.done
else
  echo "No pending Maturana run"
fi

echo "Maturana $MATURANA_HARNESS agent $MATURANA_AGENT_ID ready"
while true; do
  date -Is > /var/log/maturana/heartbeat
  sleep 60
done
RUNNER

cat > "$work_dir/maturana-agent.service" <<'SERVICE'
[Unit]
Description=Maturana Codex agent
After=network-online.target
Wants=network-online.target

[Service]
User=ubuntu
WorkingDirectory=/workspace
ExecStart=/opt/maturana/bin/run-agent.sh
Restart=on-failure
RestartSec=10
StandardOutput=append:/var/log/maturana/agent.log
StandardError=append:/var/log/maturana/agent.err.log

[Install]
WantedBy=multi-user.target
SERVICE

echo "Customizing Ubuntu image offline..."
virt-customize -a "$work_img" \
  --run-command 'apt-get update' \
  --run-command 'DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends openssh-server curl ca-certificates nodejs npm' \
  --run-command 'id ubuntu >/dev/null 2>&1 || useradd -m -s /bin/bash ubuntu' \
  --ssh-inject "ubuntu:file:$ssh_key_path.pub" \
  --run-command 'mkdir -p /etc/sudoers.d /etc/netplan /etc/cloud/cloud.cfg.d /agent /workspace /memory /wiki /opt/maturana/bin /var/log/maturana' \
  --run-command 'sed -i.bak -e "\|[[:space:]]/boot[[:space:]]|d" -e "\|[[:space:]]/boot/efi[[:space:]]|d" -e "/LABEL=BOOT/d" -e "/LABEL=UEFI/d" /etc/fstab' \
  --run-command 'printf "ubuntu ALL=(ALL) NOPASSWD: ALL\n" > /etc/sudoers.d/90-maturana-ubuntu' \
  --run-command 'chmod 0440 /etc/sudoers.d/90-maturana-ubuntu' \
  --run-command 'ssh-keygen -A' \
  --run-command 'systemctl disable ssh.socket || true' \
  --run-command 'systemctl enable ssh.service || systemctl enable ssh || true'

virt-copy-in -a "$work_img" "$work_dir/50-maturana-firecracker.yaml" /etc/netplan
virt-copy-in -a "$work_img" "$work_dir/99-disable-network-config.cfg" /etc/cloud/cloud.cfg.d
virt-copy-in -a "$work_img" "$work_dir/agent" /
virt-copy-in -a "$work_img" "$work_dir/run-agent.sh" /opt/maturana/bin
virt-copy-in -a "$work_img" "$work_dir/maturana-agent.service" /etc/systemd/system

if [[ -f "$work_dir/maturana-pipelock-ca.crt" ]]; then
  virt-copy-in -a "$work_img" "$work_dir/maturana-pipelock-ca.crt" /usr/local/share/ca-certificates
fi

if [[ -d "$auth_source" ]]; then
  mkdir -p "$work_dir/.codex"
  cp -a "$auth_source"/. "$work_dir/.codex"/
  virt-copy-in -a "$work_img" "$work_dir/.codex" /home/ubuntu
fi

virt-copy-out -a "$work_img" /boot "$work_dir"
kernel_candidate="$(find "$work_dir/boot" -maxdepth 1 -type f -name 'vmlinuz-*' | sort -V | tail -n 1)"
if [[ -z "$kernel_candidate" ]]; then
  echo "could not find vmlinuz-* in the Ubuntu image" >&2
  exit 1
fi
"$extract_vmlinux" "$kernel_candidate" > "$kernel_out"
if ! file "$kernel_out" | grep -q 'ELF'; then
  echo "failed to extract an ELF vmlinux from $kernel_candidate" >&2
  file "$kernel_out" >&2 || true
  exit 1
fi

virt-customize -a "$work_img" \
  --run-command 'chmod 0600 /etc/netplan/50-maturana-firecracker.yaml' \
  --run-command 'chmod 0755 /opt/maturana/bin/run-agent.sh' \
  --run-command 'if [ -f /usr/local/share/ca-certificates/maturana-pipelock-ca.crt ]; then update-ca-certificates; fi' \
  --run-command 'chown -R ubuntu:ubuntu /agent /workspace /memory /wiki /var/log/maturana /home/ubuntu/.codex 2>/dev/null || true' \
  --run-command 'chmod -R go-rwx /home/ubuntu/.codex 2>/dev/null || true' \
  --run-command 'systemctl enable maturana-agent.service || true' \
  --run-command 'npm install -g @openai/codex || true' \
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

if [[ "$(id -u)" -eq 0 && -n "${SUDO_UID:-}" && -n "${SUDO_GID:-}" ]]; then
  chown -R "$SUDO_UID:$SUDO_GID" "$output_dir"
fi
