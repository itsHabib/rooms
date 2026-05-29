#!/usr/bin/env bash
# Boot smoke test for a rooms Alpine agent rootfs image.
#
# Boots the image under Firecracker with a sibling vmlinux.bin, waits for sshd,
# and asserts the agent contract: key-only login as the unprivileged rooms user,
# DNS working out of the box, claude-code + git present with claude linking
# cleanly against musl, virtio-rng, TLS to the Anthropic API, fast boot, and a
# content size well under 300 MB.
#
# Requires: firecracker, curl, jq, ssh, ip, a sibling vmlinux.bin, tap-fc0
# (scripts/setup-tap.sh), and the private key matching the pubkey baked at build.
#
# Usage:
#   ./scripts/test-rootfs-alpine.sh [rootfs-path]
#   SSH_KEY=~/.ssh/id_rooms ./scripts/test-rootfs-alpine.sh images/agent-alpine.ext4

set -euo pipefail

ROOTFS="${1:-images/agent-alpine.ext4}"
KEY_PATH="${SSH_KEY:-${KEY_PATH:-$HOME/.ssh/id_rooms}}"
GUEST_USER="${GUEST_USER:-rooms}"
GUEST_IP="${GUEST_IP:-172.16.0.2}"
GATEWAY_IP="${GATEWAY_IP:-172.16.0.1}"
NETMASK="${NETMASK:-255.255.255.0}"
TAP="${TAP:-tap-fc0}"
BOOT_WAIT="${BOOT_WAIT:-20}"
MAX_MB="${MAX_MB:-300}"

FC_PID=""
ROOM_DIR=""
SOCKET=""

log()   { printf '\033[1;34m[test-alpine]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[test-alpine]\033[0m %s\n' "$*" >&2; exit 1; }

cleanup() {
    if [[ -n "$FC_PID" ]] && kill -0 "$FC_PID" 2>/dev/null; then
        kill "$FC_PID" 2>/dev/null || true
        wait "$FC_PID" 2>/dev/null || true
    fi
    [[ -n "$ROOM_DIR" && -d "$ROOM_DIR" ]] && rm -rf "$ROOM_DIR"
}
trap cleanup EXIT INT TERM

for cmd in firecracker curl ssh ip jq file; do
    command -v "$cmd" >/dev/null 2>&1 || fatal "missing required command: $cmd"
done

[[ -f "$ROOTFS" ]] || fatal "rootfs not found: $ROOTFS"
ROOTFS="$(readlink -f "$ROOTFS")"
[[ -f "$KEY_PATH" ]] || fatal "ssh private key not found: $KEY_PATH (set SSH_KEY=...)"

KERNEL="$(dirname "$ROOTFS")/vmlinux.bin"
[[ -f "$KERNEL" ]] || fatal "kernel not found at $KERNEL (run scripts/setup-rooms-host.sh)"
KERNEL="$(readlink -f "$KERNEL")"
file "$KERNEL" | grep -q 'ELF 64-bit' || fatal "kernel $KERNEL is not an uncompressed ELF vmlinux"

ip link show "$TAP" >/dev/null 2>&1 || fatal "TAP $TAP not found; run: sudo bash scripts/setup-tap.sh"

ROOM_DIR="$(mktemp -d -t rooms-test-alpine.XXXXXX)"
SOCKET="$ROOM_DIR/api.sock"
LOG="$ROOM_DIR/firecracker.log"

api_put() {
    curl --unix-socket "$SOCKET" -X PUT -H 'Content-Type: application/json' \
        -d "$2" --fail-with-body --silent --show-error "http://localhost$1" >/dev/null
}
wait_for_socket() {
    local deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)); do
        curl --unix-socket "$SOCKET" --silent http://localhost/ >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    fatal "firecracker api socket never became ready; see $LOG"
}
ssh_guest() {
    ssh -i "$KEY_PATH" -o BatchMode=yes -o ConnectTimeout=5 \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
        "${GUEST_USER}@${GUEST_IP}" "$@"
}

log "booting $ROOTFS (kernel $KERNEL)"
firecracker --api-sock "$SOCKET" >"$LOG" 2>&1 &
FC_PID=$!
wait_for_socket

BOOT_ARGS="console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on ip=${GUEST_IP}::${GATEWAY_IP}:${NETMASK}::eth0:off"
api_put /boot-source "$(jq -nc --arg k "$KERNEL" --arg b "$BOOT_ARGS" '{kernel_image_path:$k, boot_args:$b}')"
api_put /drives/rootfs "$(jq -nc --arg p "$ROOTFS" '{drive_id:"rootfs", path_on_host:$p, is_root_device:true, is_read_only:false}')"
api_put /machine-config '{"vcpu_count":1,"mem_size_mib":512}'
api_put /network-interfaces/eth0 "$(jq -nc --arg t "$TAP" '{iface_id:"eth0", host_dev_name:$t}')"
api_put /entropy '{}'
START="$(date +%s%3N)"
api_put /actions '{"action_type":"InstanceStart"}'

log "waiting for sshd (up to ${BOOT_WAIT}s) as ${GUEST_USER}@${GUEST_IP}"
deadline=$((SECONDS + BOOT_WAIT))
booted=""
while ((SECONDS < deadline)); do
    if ssh_guest true >/dev/null 2>&1; then
        booted="$(( $(date +%s%3N) - START ))"
        break
    fi
    sleep 0.5
done
[[ -n "$booted" ]] || fatal "sshd did not accept ${GUEST_USER} pubkey within ${BOOT_WAIT}s; see $LOG"
log "sshd reachable in ${booted}ms"
((booted < 5000)) || log "WARN: boot-to-sshd ${booted}ms exceeds the 5s target"

log "checking guest user + tooling"
ssh_guest "set -e
    [ \"\$(id -un)\" = '${GUEST_USER}' ] || { echo \"wrong user: \$(id -un)\"; exit 1; }
    command -v git    >/dev/null || { echo 'git missing';    exit 1; }
    command -v claude >/dev/null || { echo 'claude missing'; exit 1; }"

log "checking DNS resolves with no manual fix"
ssh_guest 'getent hosts github.com >/dev/null' || fatal "getent hosts github.com failed in guest"

log "checking virtio-rng (/dev/hwrng present)"
ssh_guest 'test -e /dev/hwrng' || fatal "/dev/hwrng missing (kernel lacks virtio-rng?)"

log "checking claude links against musl (no relocation errors)"
CLAUDE_OUT="$(ssh_guest 'claude --version 2>&1')" || fatal "claude --version failed: $CLAUDE_OUT"
if printf '%s' "$CLAUDE_OUT" | grep -qiE 'symbol not found|Error relocating'; then
    fatal "glibc symbol leaked into the musl claude binary: $CLAUDE_OUT"
fi
log "claude: $CLAUDE_OUT"

log "checking TLS to api.anthropic.com (ca-certificates)"
# A non-2xx HTTP status (e.g. 404 at the root path) still proves the ca bundle
# verified the cert and TLS completed; only a transport/TLS failure is fatal.
ssh_guest 'curl -sS --max-time 10 -o /dev/null https://api.anthropic.com/' \
    || fatal "TLS to api.anthropic.com failed (ca bundle / cert verification?)"

log "checking sshd refuses password auth"
PWAUTH="$(ssh_guest "sudo sshd -T 2>/dev/null | awk '/^passwordauthentication/ {print \$2}'")"
[[ "$PWAUTH" == "no" ]] || fatal "passwordauthentication is '$PWAUTH' (expected 'no')"

# Allocated (not apparent) size — the image is sparse, so this tracks content.
SIZE_B="$(du --block-size=1 "$ROOTFS" | awk '{print $1}')"
SIZE_H="$(du -h "$ROOTFS" | awk '{print $1}')"
MAX_B=$((MAX_MB * 1024 * 1024))
log "image size: $SIZE_H allocated (limit ${MAX_MB}M)"
((SIZE_B < MAX_B)) || fatal "image too large: ${SIZE_H} (limit ${MAX_MB}M)"

log "smoke test passed"
