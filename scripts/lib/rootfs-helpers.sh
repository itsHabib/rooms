#!/usr/bin/env bash
# Shared helpers for scripts/build-rootfs.sh. Sourced, not executed directly.

# shellcheck shell=bash

# Exit 1 unless running as root (debootstrap and loop mounts require it).
assert_root() {
    if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
        printf '\033[1;31m[build-rootfs]\033[0m must run as root; re-run with sudo\n' >&2
        exit 1
    fi
}

# Unmount ${mnt} and detach ${loop} if still attached. Idempotent — safe in traps.
cleanup_mount() {
    local mnt="${1:-}"
    local loop="${2:-}"

    if [[ -n "$mnt" ]]; then
        if mountpoint -q "$mnt/dev/pts" 2>/dev/null; then
            umount "$mnt/dev/pts" 2>/dev/null || true
        fi
        for sub in sys proc dev; do
            if mountpoint -q "$mnt/$sub" 2>/dev/null; then
                umount "$mnt/$sub" 2>/dev/null || true
            fi
        done
        if mountpoint -q "$mnt" 2>/dev/null; then
            umount "$mnt" 2>/dev/null || true
        fi
        if [[ -d "$mnt" ]]; then
            rmdir "$mnt" 2>/dev/null || true
        fi
    fi
    if [[ -n "$loop" ]] && losetup "$loop" >/dev/null 2>&1; then
        losetup -d "$loop" 2>/dev/null || true
    fi
}

# Write rooms apt defaults into a mounted rootfs (before or during chroot).
pin_chroot_apt() {
    local mnt="$1"
    install -d "$mnt/etc/apt/apt.conf.d"
    cat >"$mnt/etc/apt/apt.conf.d/99rooms" <<'EOF'
Acquire::Retries "3";
APT::Get::Assume-Yes "true";
EOF
}
