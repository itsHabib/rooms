#!/usr/bin/env bash
# setup-rooms-host.sh — run INSIDE the Ubuntu VM that hosts rooms.
#
# Installs Firecracker, the quickstart kernel + rootfs, Rust, Node + claude-code
# CLI; verifies /dev/kvm; sets up the work-dir layout. Idempotent — re-running
# this script after a partial install is safe.
#
# Run as the 'rooms' user (or any non-root user with sudo). Uses sudo for the
# steps that require root.

set -euo pipefail

# --- config ---

FIRECRACKER_VERSION="${FIRECRACKER_VERSION:-v1.10.1}"
RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}"
NODE_MAJOR="${NODE_MAJOR:-20}"
IMAGES_DIR="${IMAGES_DIR:-$HOME/rooms/images}"
ARCH="$(uname -m)"

# Firecracker quickstart kernel + rootfs URLs (x86_64).
# These are the well-known community images suitable for getting Firecracker
# to boot once. Production rootfs will be built by scripts/build-rootfs.sh
# (task #6); these are for POC only.
QUICKSTART_KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/x86_64/kernels/vmlinux.bin"
QUICKSTART_ROOTFS_URL="https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/x86_64/rootfs/bionic.rootfs.ext4"

# --- helpers ---

log()   { printf '\033[1;34m[setup]\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m[warn ]\033[0m %s\n' "$*" >&2; }
fatal() { printf '\033[1;31m[fatal]\033[0m %s\n' "$*" >&2; exit 1; }

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || fatal "missing required command: $1"
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
    curl ca-certificates gnupg jq \
    git build-essential pkg-config libssl-dev \
    iproute2 iputils-ping bridge-utils \
    e2fsprogs

# --- Firecracker ---

if command -v firecracker >/dev/null 2>&1; then
    log "Firecracker already installed: $(firecracker --version)"
else
    log "installing Firecracker $FIRECRACKER_VERSION"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    url="https://github.com/firecracker-microvm/firecracker/releases/download/${FIRECRACKER_VERSION}/firecracker-${FIRECRACKER_VERSION}-x86_64.tgz"
    curl -fSL "$url" -o "$tmp/fc.tgz"
    tar -C "$tmp" -xzf "$tmp/fc.tgz"
    sudo install -m 0755 "$tmp"/release-${FIRECRACKER_VERSION}-x86_64/firecracker-${FIRECRACKER_VERSION}-x86_64 /usr/local/bin/firecracker
    sudo install -m 0755 "$tmp"/release-${FIRECRACKER_VERSION}-x86_64/jailer-${FIRECRACKER_VERSION}-x86_64 /usr/local/bin/jailer
    log "Firecracker installed: $(firecracker --version)"
fi

# --- quickstart kernel + rootfs ---

mkdir -p "$IMAGES_DIR"

if [[ -f "$IMAGES_DIR/vmlinux.bin" ]]; then
    log "kernel image already present: $IMAGES_DIR/vmlinux.bin"
else
    log "downloading quickstart kernel"
    curl -fSL "$QUICKSTART_KERNEL_URL" -o "$IMAGES_DIR/vmlinux.bin"
fi

if [[ -f "$IMAGES_DIR/rootfs.ext4" ]]; then
    log "rootfs image already present: $IMAGES_DIR/rootfs.ext4"
else
    log "downloading quickstart rootfs (this is the throwaway POC image; task #6 replaces it)"
    curl -fSL "$QUICKSTART_ROOTFS_URL" -o "$IMAGES_DIR/rootfs.ext4"
fi

# --- Rust ---

if command -v cargo >/dev/null 2>&1; then
    log "Rust already installed: $(cargo --version)"
else
    log "installing Rust via rustup ($RUSTUP_TOOLCHAIN)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain "$RUSTUP_TOOLCHAIN"
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
fi

# --- Node + claude-code ---

if command -v node >/dev/null 2>&1 && [[ "$(node -v)" =~ ^v${NODE_MAJOR}\. ]]; then
    log "Node $NODE_MAJOR already installed: $(node -v)"
else
    log "installing Node $NODE_MAJOR via NodeSource"
    curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | sudo -E bash -
    sudo apt-get install -y -qq nodejs
fi

if command -v claude >/dev/null 2>&1; then
    log "claude-code already installed: $(claude --version 2>/dev/null || echo present)"
else
    log "installing @anthropic-ai/claude-code globally"
    sudo npm install -g @anthropic-ai/claude-code
fi

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
    log "CURSOR_API_KEY is not set — only needed once task #4 (cursor SDK runner) lands"
fi

# --- summary ---

log ""
log "setup complete."
log ""
log "  firecracker: $(firecracker --version 2>/dev/null || echo MISSING)"
log "  kernel:      $IMAGES_DIR/vmlinux.bin"
log "  rootfs:      $IMAGES_DIR/rootfs.ext4"
log "  rust:        $(cargo --version 2>/dev/null || echo MISSING)"
log "  node:        $(node --version 2>/dev/null || echo MISSING)"
log "  claude:      $(command -v claude >/dev/null && echo present || echo MISSING)"
log ""
log "next:"
log "  1. cd ~/rooms && make check         (sanity-check the toolchain)"
log "  2. start writing the Firecracker boot code in src/firecracker.rs"
log "  3. POC target: rooms run --repo <path> --task <task.md> → microVM + patch out"
log ""
