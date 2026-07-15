#!/usr/bin/env bash
# setup-rooms-host.sh — run INSIDE the Ubuntu VM that hosts rooms.
#
# Installs Firecracker, the quickstart kernel + rootfs, Rust; verifies /dev/kvm;
# sets up the work-dir layout. The agent binary (claude-code) lives in the guest
# rootfs, not on the host. Idempotent — re-running after a partial install is safe.
#
# Run as the 'rooms' user (or any non-root user with sudo). Uses sudo for the
# steps that require root.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKSUMS="${SCRIPT_DIR}/checksums.txt"

# --- config ---

FIRECRACKER_VERSION="${FIRECRACKER_VERSION:-v1.10.1}"
RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}"
IMAGES_DIR="${IMAGES_DIR:-$HOME/rooms/images}"
ARCH="$(uname -m)"

# Firecracker CI guest kernel (x86_64, uncompressed vmlinux) with virtio-rng
# built in, so the guest CRNG seeds from the /entropy device with no host-side
# workaround. Pinned; bump deliberately (CI_VERSION tracks the firecracker
# release minor, e.g. v1.15.x -> v1.15).
FC_KERNEL_CI_VERSION="${FC_KERNEL_CI_VERSION:-v1.15}"
FC_KERNEL_VERSION="${FC_KERNEL_VERSION:-6.1.155}"
FC_KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/${FC_KERNEL_CI_VERSION}/x86_64/vmlinux-${FC_KERNEL_VERSION}"
# Quickstart bionic rootfs — a throwaway image to boot Firecracker once before
# building a real agent rootfs with scripts/build-rootfs-alpine.sh.
QUICKSTART_ROOTFS_URL="https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/x86_64/rootfs/bionic.rootfs.ext4"

# --- helpers ---

log()   { printf '\033[1;34m[setup]\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m[warn ]\033[0m %s\n' "$*" >&2; }
fatal() { printf '\033[1;31m[fatal]\033[0m %s\n' "$*" >&2; exit 1; }

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || fatal "missing required command: $1"
}

lookup_checksum() {
    local artifact="$1"
    awk -v name="$artifact" '{ sub(/\r$/, "", $2) } $1 ~ /^[0-9a-f]{64}$/ && $2 == name { print $1; exit }' "$CHECKSUMS"
}

verify_sha256() {
    local file="$1" artifact="$2"
    [[ -f "$CHECKSUMS" ]] || fatal "checksums file not found: $CHECKSUMS"
    local expected actual
    expected="$(lookup_checksum "$artifact")"
    [[ -n "$expected" ]] || fatal "no sha256 pin for $artifact in $CHECKSUMS"
    actual="$(sha256sum "$file" | awk '{print $1}')"
    if [[ "$actual" != "$expected" ]]; then
        fatal "sha256 mismatch for $artifact: expected $expected, got $actual (see $CHECKSUMS)"
    fi
}

# --- preflight ---

log "preflight checks"

if [[ "$ARCH" != "x86_64" ]]; then
    fatal "this script targets x86_64; detected $ARCH"
fi

if [[ "$EUID" -eq 0 ]]; then
    warn "running as root — recommended to run as the 'rooms' user; will continue"
fi

if ! sudo -n true 2>/dev/null; then
    log "sudo will prompt for your password during install steps"
fi

require_cmd sha256sum

# --- /dev/kvm ---

log "verifying /dev/kvm (nested virt)"
if [[ ! -e /dev/kvm ]]; then
    fatal "/dev/kvm not found — nested virtualization is OFF.
    On the Windows host, with the VM shut down:
      Set-VMProcessor -VMName rooms-host -ExposeVirtualizationExtensions \$true
    Then power the VM back on and re-run this script."
fi

if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
    log "/dev/kvm exists but is not r/w by current user; adding $USER to 'kvm' group"
    sudo usermod -aG kvm "$USER"
    warn "you need to log out and back in (or 'newgrp kvm') for group membership to take effect"
fi

# --- apt baseline ---

log "installing baseline apt packages"
sudo apt-get update -qq
sudo apt-get install -y -qq \
    curl ca-certificates gnupg jq file \
    git build-essential pkg-config libssl-dev \
    iproute2 iputils-ping bridge-utils \
    e2fsprogs

# --- Firecracker system user ---

log "ensuring dedicated ${FIRECRACKER_USER:-firecracker} system user"
FIRECRACKER_USER="${FIRECRACKER_USER:-firecracker}"
if id "$FIRECRACKER_USER" >/dev/null 2>&1; then
    log "$FIRECRACKER_USER user already exists"
else
    sudo useradd --system --no-create-home --shell /usr/sbin/nologin "$FIRECRACKER_USER"
    log "created system user $FIRECRACKER_USER"
fi

# --- Firecracker ---

if command -v firecracker >/dev/null 2>&1; then
    log "Firecracker already installed: $(firecracker --version)"
else
    log "installing Firecracker $FIRECRACKER_VERSION"
    tmp="$(mktemp -d)"
    url="https://github.com/firecracker-microvm/firecracker/releases/download/${FIRECRACKER_VERSION}/firecracker-${FIRECRACKER_VERSION}-x86_64.tgz"
    curl -fSL "$url" -o "$tmp/fc.tgz"
    verify_sha256 "$tmp/fc.tgz" "firecracker-${FIRECRACKER_VERSION}-x86_64.tgz"
    tar -C "$tmp" -xzf "$tmp/fc.tgz"
    sudo install -m 0755 "$tmp"/release-${FIRECRACKER_VERSION}-x86_64/firecracker-${FIRECRACKER_VERSION}-x86_64 /usr/local/bin/firecracker
    sudo install -m 0755 "$tmp"/release-${FIRECRACKER_VERSION}-x86_64/jailer-${FIRECRACKER_VERSION}-x86_64 /usr/local/bin/jailer
    rm -rf "$tmp"
    log "Firecracker installed: $(firecracker --version)"
fi

# --- quickstart kernel + rootfs ---

mkdir -p "$IMAGES_DIR"

KERNEL_FILE="$IMAGES_DIR/vmlinux-${FC_KERNEL_VERSION}.bin"
if [[ ! -f "$KERNEL_FILE" ]]; then
    log "downloading Firecracker CI kernel ${FC_KERNEL_VERSION} (virtio-rng built in)"
    # Atomic .tmp -> mv so an interrupted download never persists as the
    # versioned kernel (a later run would otherwise skip the download and adopt
    # the partial file).
    curl -fSL "$FC_KERNEL_URL" -o "$KERNEL_FILE.tmp"
    verify_sha256 "$KERNEL_FILE.tmp" "vmlinux-${FC_KERNEL_VERSION}.bin"
    if ! file "$KERNEL_FILE.tmp" | grep -q 'ELF 64-bit'; then
        rm -f "$KERNEL_FILE.tmp"
        fatal "downloaded kernel is not an uncompressed ELF vmlinux: $FC_KERNEL_URL"
    fi
    mv "$KERNEL_FILE.tmp" "$KERNEL_FILE"
else
    log "kernel image already present: $KERNEL_FILE"
fi
# Validate the kernel (cached or freshly downloaded) before adopting it, and
# always point vmlinux.bin at it so a host that still has the old bionic
# vmlinux.bin picks up the virtio-rng kernel instead of silently keeping it.
file "$KERNEL_FILE" | grep -q 'ELF 64-bit' \
    || fatal "cached kernel is not an uncompressed ELF vmlinux: $KERNEL_FILE"
cp -f "$KERNEL_FILE" "$IMAGES_DIR/vmlinux.bin"

if [[ -f "$IMAGES_DIR/rootfs.ext4" ]]; then
    log "rootfs image already present: $IMAGES_DIR/rootfs.ext4"
else
    log "downloading quickstart rootfs (throwaway POC image; build-rootfs-alpine.sh replaces it)"
    curl -fSL "$QUICKSTART_ROOTFS_URL" -o "$IMAGES_DIR/rootfs.ext4.tmp"
    verify_sha256 "$IMAGES_DIR/rootfs.ext4.tmp" "bionic.rootfs.ext4"
    mv "$IMAGES_DIR/rootfs.ext4.tmp" "$IMAGES_DIR/rootfs.ext4"
fi

# Kernel + rootfs must be readable by the jailed firecracker user.
if id "$FIRECRACKER_USER" >/dev/null 2>&1; then
    for img in "$IMAGES_DIR/vmlinux.bin" "$IMAGES_DIR/rootfs.ext4"; do
        [[ -f "$img" ]] && sudo chgrp "$FIRECRACKER_USER" "$img"
    done
    # Kernel is read-only; the rootfs is opened read-write by the jailed
    # firecracker user (is_read_only: false in configure_vm), so group read
    # alone makes the drive attach fail — it needs group write too.
    [[ -f "$IMAGES_DIR/vmlinux.bin" ]] && sudo chmod g+r "$IMAGES_DIR/vmlinux.bin"
    [[ -f "$IMAGES_DIR/rootfs.ext4" ]] && sudo chmod g+rw "$IMAGES_DIR/rootfs.ext4"
    log "kernel group-readable + rootfs group-read-write by $FIRECRACKER_USER"
fi

# --- Rust ---

if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
fi

if command -v cargo >/dev/null 2>&1; then
    log "Rust already installed: $(cargo --version)"
else
    log "installing Rust via rustup ($RUSTUP_TOOLCHAIN, profile minimal)"
    # canonical TLS-authenticated installer; intentionally unpinned (unlike the
    # artifacts above) — the wrapper fetches an unpinned rustup-init at runtime.
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain "$RUSTUP_TOOLCHAIN" --profile minimal
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
fi

# clippy + rustfmt on every path (fresh install or re-run where cargo already
# exists); guarded so a non-rustup cargo (distro package) can't abort the run.
command -v rustup >/dev/null 2>&1 && rustup component add clippy rustfmt

# --- work dir layout ---

WORK_ROOT="$HOME/.local/state/rooms"
mkdir -p "$WORK_ROOT"
log "per-room work dir root: $WORK_ROOT"

# --- env hints ---

log "checking required env vars"
if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    warn "ANTHROPIC_API_KEY is not set — needed for 'claude -p' inside the room"
    warn "  add to ~/.bashrc:   export ANTHROPIC_API_KEY=\"sk-ant-...\""
fi
if [[ -z "${CURSOR_API_KEY:-}" ]]; then
    log "CURSOR_API_KEY is not set — required only for --runner cursor"
fi

# --- summary ---

log ""
log "setup complete."
log ""
log "  firecracker: $(firecracker --version 2>/dev/null || echo MISSING)"
log "  kernel:      $IMAGES_DIR/vmlinux.bin"
log "  rootfs:      $IMAGES_DIR/rootfs.ext4"
log "  rust:        $(cargo --version 2>/dev/null || echo MISSING)"
log ""
log "next:"
log "  1. cd ${SCRIPT_DIR}/.. && make check"
log "  2. create the guest key if absent: ssh-keygen -q -t ed25519 -N '' -f ~/.ssh/id_rooms"
log "  3. build the canonical image: sudo ./scripts/build-rootfs-alpine.sh --out $IMAGES_DIR/rootfs.ext4 --ssh-key ~/.ssh/id_rooms.pub"
log "  4. smoke test: ./scripts/test-rootfs-alpine.sh $IMAGES_DIR/rootfs.ext4"
log ""
