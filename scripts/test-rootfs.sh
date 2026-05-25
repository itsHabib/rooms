#!/usr/bin/env bash
# Boot smoke test for a rooms-built rootfs image.
#
# Requires: firecracker, curl, tap-fc0 (scripts/setup-tap.sh), sibling vmlinux.bin,
# and an SSH private key matching the pubkey baked at build time.
#
# Usage:
#   ./scripts/test-rootfs.sh [rootfs-path]
#   SSH_KEY=~/.ssh/id_rooms ./scripts/test-rootfs.sh images/node-dev.ext4

set -euo pipefail

ROOTFS="${1:-images/node-dev.ext4}"
KEY_PATH="${SSH_KEY:-${KEY_PATH:-$HOME/.ssh/id_rooms}}"
GUEST_IP="${GUEST_IP:-172.16.0.2}"
GATEWAY_IP="${GATEWAY_IP:-172.16.0.1}"
NETMASK="${NETMASK:-255.255.255.0}"
TAP="${TAP:-tap-fc0}"
BOOT_WAIT="${BOOT_WAIT:-30}"

FC_PID=""
ROOM_DIR=""
SOCKET=""

log()   { printf '\033[1;34m[test-rootfs]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[test-rootfs]\033[0m %s\n' "$*" >&2; exit 1; }

cleanup() {
    if [[ -n "$FC_PID" ]] && kill -0 "$FC_PID" 2>/dev/null; then
        kill "$FC_PID" 2>/dev/null || true
        wait "$FC_PID" 2>/dev/null || true
    fi
    if [[ -n "$ROOM_DIR" && -d "$ROOM_DIR" ]]; then
        rm -rf "$ROOM_DIR"
    fi
}
trap cleanup EXIT INT TERM

for cmd in firecracker curl ssh ip jq; do
    command -v "$cmd" >/dev/null 2>&1 || fatal "missing required command: $cmd"
done

if [[ ! -f "$ROOTFS" ]]; then
    fatal "rootfs not found: $ROOTFS"
fi
ROOTFS="$(readlink -f "$ROOTFS")"
if [[ ! -f "$KEY_PATH" ]]; then
    fatal "ssh private key not found: $KEY_PATH (set SSH_KEY=...)"
fi

KERNEL="$(dirname "$ROOTFS")/vmlinux.bin"
if [[ ! -f "$KERNEL" ]]; then
    fatal "kernel not found at $KERNEL (expected sibling of rootfs; run scripts/setup-rooms-host.sh)"
fi
KERNEL="$(readlink -f "$KERNEL")"

if ! ip link show "$TAP" >/dev/null 2>&1; then
    fatal "TAP $TAP not found; run: sudo bash scripts/setup-tap.sh"
fi

ROOM_DIR="$(mktemp -d -t rooms-test.XXXXXX)"
SOCKET="$ROOM_DIR/api.sock"
LOG="$ROOM_DIR/firecracker.log"

api_put() {
    local endpoint="$1"
    local body="$2"
    curl --unix-socket "$SOCKET" -X PUT \
        -H 'Content-Type: application/json' \
        -d "$body" \
        --fail-with-body --silent --show-error \
        "http://localhost${endpoint}" >/dev/null
}

wait_for_socket() {
    local deadline=$((SECONDS + 10))
    while (( SECONDS < deadline )); do
        if curl --unix-socket "$SOCKET" --silent --show-error http://localhost/ >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    fatal "firecracker api socket did not become ready; see $LOG"
}

ssh_rooms() {
    ssh -i "$KEY_PATH" \
        -o BatchMode=yes \
        -o ConnectTimeout=5 \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR \
        "rooms@${GUEST_IP}" \
        "$@"
}

wait_for_ssh() {
    local deadline=$((SECONDS + BOOT_WAIT))
    while (( SECONDS < deadline )); do
        if ssh_rooms true >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    fatal "sshd did not accept pubkey auth for rooms@${GUEST_IP} within ${BOOT_WAIT}s"
}

log "booting $ROOTFS via firecracker (log: $LOG)"
firecracker --api-sock "$SOCKET" >"$LOG" 2>&1 &
FC_PID=$!
wait_for_socket

BOOT_ARGS="console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on ip=${GUEST_IP}::${GATEWAY_IP}:${NETMASK}::eth0:off"
api_put /boot-source "$(jq -nc --arg k "$KERNEL" --arg b "$BOOT_ARGS" \
    '{kernel_image_path:$k, boot_args:$b}')"
api_put /drives/rootfs "$(jq -nc --arg p "$ROOTFS" \
    '{drive_id:"rootfs", path_on_host:$p, is_root_device:true, is_read_only:false}')"
api_put /machine-config '{"vcpu_count":1,"mem_size_mib":512}'
api_put /network-interfaces/eth0 "$(jq -nc --arg t "$TAP" \
    '{iface_id:"eth0", host_dev_name:$t}')"
api_put /entropy '{}'
api_put /actions '{"action_type":"InstanceStart"}'

log "waiting for sshd (up to ${BOOT_WAIT}s)"
wait_for_ssh

log "running guest sanity checks"
for cmd in 'which git' 'which node' 'which claude' 'id rooms'; do
    log "  $cmd"
    ssh_rooms "$cmd"
done

log "checking sshd config disables password auth"
# A BatchMode=yes ssh probe with PubkeyAuthentication=no would fail
# unconditionally (ssh refuses to prompt non-interactively), so it can't
# distinguish a hardened sshd from a default one. Inspect the running
# config from inside the guest instead.
PWAUTH="$(ssh_rooms "sudo sshd -T 2>/dev/null | awk '/^passwordauthentication/ {print \$2}'")"
if [[ "$PWAUTH" != "no" ]]; then
    fatal "passwordauthentication is '$PWAUTH' (expected 'no')"
fi

# Compare allocated on-disk bytes, not apparent size. `du --bytes` is
# actually an alias for `--apparent-size --block-size=1`, which would
# report the full ext4 capacity (e.g. 4G) on a sparse image. Default
# `du --block-size=1` reports allocated bytes (= roughly the rootfs
# content size for a sparse + mkfs.ext4 image).
SIZE_B="$(du --block-size=1 "$ROOTFS" | awk '{print $1}')"
SIZE_H="$(du -h "$ROOTFS" | awk '{print $1}')"
log "image size: $SIZE_H allocated (expect < 1.5G)"
MAX_B=$((1536 * 1024 * 1024))
if (( SIZE_B >= MAX_B )); then
    fatal "image too large: ${SIZE_H} (limit 1.5G)"
fi

log "smoke test passed"
