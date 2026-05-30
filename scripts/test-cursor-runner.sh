#!/usr/bin/env bash
# End-to-end dogfood for the cursor SDK runner baked into an agent image.
#
# Boots the image under Firecracker, seeds a fixture git repo in
# /workspace/repo, stages /workspace/in, runs cursor-runner.js as the rooms
# user, and asserts the runner contract:
#   - success path: a run with CURSOR_API_KEY round-trips a patch (README.md
#     changes), events.ndjson is valid NDJSON ending in a succeeded result, and
#     summary.md is non-empty;
#   - auth-failure path: a run with no CURSOR_API_KEY exits 2 and writes a
#     structured { kind:"error", phase:"api_key" } line to events.ndjson.
#
# This exercises cursor-runner.js + the vendored @cursor/sdk inside the guest
# (musl), and the SSH SendEnv/AcceptEnv key forwarding the Rust runner relies on.
#
# Requires: firecracker, curl, jq, ssh, ip, file, a sibling vmlinux.bin, tap-fc0
# (scripts/setup-tap.sh), the private key matching the baked pubkey, and
# CURSOR_API_KEY in the environment (for the success path).
#
# Usage:
#   CURSOR_API_KEY=... ./scripts/test-cursor-runner.sh [rootfs-path]
#   MODEL=composer-2.5 SSH_KEY=~/.ssh/id_rooms ./scripts/test-cursor-runner.sh images/agent-alpine-cursor.ext4

set -euo pipefail

ROOTFS="${1:-images/agent-alpine-cursor.ext4}"
KEY_PATH="${SSH_KEY:-${KEY_PATH:-$HOME/.ssh/id_rooms}}"
GUEST_USER="${GUEST_USER:-rooms}"
GUEST_IP="${GUEST_IP:-172.16.0.2}"
GATEWAY_IP="${GATEWAY_IP:-172.16.0.1}"
NETMASK="${NETMASK:-255.255.255.0}"
TAP="${TAP:-tap-fc0}"
BOOT_WAIT="${BOOT_WAIT:-20}"
MEM_MIB="${MEM_MIB:-1024}"
MODEL="${MODEL:-composer-2.5}"
RUNNER_JS="/opt/rooms/cursor-runner/cursor-runner.js"

FC_PID=""
ROOM_DIR=""
SOCKET=""

log()   { printf '\033[1;34m[test-cursor]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[test-cursor]\033[0m %s\n' "$*" >&2; exit 1; }

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
[[ -n "${CURSOR_API_KEY:-}" ]] || fatal "CURSOR_API_KEY not set (needed for the success path)"

KERNEL="$(dirname "$ROOTFS")/vmlinux.bin"
[[ -f "$KERNEL" ]] || fatal "kernel not found at $KERNEL"
KERNEL="$(readlink -f "$KERNEL")"

ip link show "$TAP" >/dev/null 2>&1 || fatal "TAP $TAP not found; run: sudo bash scripts/setup-tap.sh"

ROOM_DIR="$(mktemp -d -t rooms-cursor.XXXXXX)"
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
# ssh_key forwards CURSOR_API_KEY (success path); ssh_nokey does not (auth fail).
ssh_opts=(-i "$KEY_PATH" -o BatchMode=yes -o ConnectTimeout=5
    -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
ssh_key()   { ssh "${ssh_opts[@]}" -o SendEnv=CURSOR_API_KEY "${GUEST_USER}@${GUEST_IP}" "$@"; }
ssh_nokey() { ssh "${ssh_opts[@]}" "${GUEST_USER}@${GUEST_IP}" "$@"; }
put_guest() { ssh_key "mkdir -p \"\$(dirname '$1')\" && cat > '$1'"; }

log "booting $ROOTFS (kernel $KERNEL, ${MEM_MIB} MiB)"
firecracker --api-sock "$SOCKET" >"$LOG" 2>&1 &
FC_PID=$!
wait_for_socket

BOOT_ARGS="console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on ip=${GUEST_IP}::${GATEWAY_IP}:${NETMASK}::eth0:off"
api_put /boot-source "$(jq -nc --arg k "$KERNEL" --arg b "$BOOT_ARGS" '{kernel_image_path:$k, boot_args:$b}')"
api_put /drives/rootfs "$(jq -nc --arg p "$ROOTFS" '{drive_id:"rootfs", path_on_host:$p, is_root_device:true, is_read_only:false}')"
api_put /machine-config "$(jq -nc --argjson m "$MEM_MIB" '{vcpu_count:1, mem_size_mib:$m}')"
api_put /network-interfaces/eth0 "$(jq -nc --arg t "$TAP" '{iface_id:"eth0", host_dev_name:$t}')"
api_put /entropy '{}'
api_put /actions '{"action_type":"InstanceStart"}'

log "waiting for sshd (up to ${BOOT_WAIT}s) as ${GUEST_USER}@${GUEST_IP}"
deadline=$((SECONDS + BOOT_WAIT))
booted=""
while ((SECONDS < deadline)); do
    if ssh_nokey true >/dev/null 2>&1; then booted=1; break; fi
    sleep 0.5
done
[[ -n "$booted" ]] || fatal "sshd did not accept ${GUEST_USER} pubkey within ${BOOT_WAIT}s; see $LOG"
log "sshd reachable"

log "seeding fixture repo in /workspace/repo"
ssh_key 'set -e
    rm -rf /workspace/repo && mkdir -p /workspace/repo && cd /workspace/repo
    git init -q
    git config user.email tester@example.com && git config user.name tester
    printf "# fixture\n\nseeded by test-cursor-runner.sh\n" > README.md
    git add -A && git commit -qm "init fixture"'
BASE_SHA="$(ssh_key 'git -C /workspace/repo rev-parse HEAD')"
log "fixture base_sha=$BASE_SHA"

log "staging /workspace/in (task.md + meta.json, model=$MODEL)"
printf 'Append a single new line that reads exactly "# rooms-was-here" to the end of README.md in this repository. Edit the file directly and save it.\n' \
    | put_guest /workspace/in/task.md
printf '{"base_sha":"%s","model_id":"%s"}\n' "$BASE_SHA" "$MODEL" \
    | put_guest /workspace/in/meta.json

log "running cursor-runner.js (success path) — this calls the real cursor agent"
run_exit=0
ssh_key "node $RUNNER_JS < /dev/null" || run_exit=$?
log "cursor-runner.js exit: $run_exit"

EVENTS="$(ssh_key 'cat /workspace/out/events.ndjson 2>/dev/null || true')"
log "events.ndjson:"
printf '%s\n' "$EVENTS"
SUMMARY="$(ssh_key 'cat /workspace/out/summary.md 2>/dev/null || true')"
log "summary.md (first 20 lines):"
printf '%s\n' "$SUMMARY" | head -20

[[ "$run_exit" -eq 0 ]] || fatal "success path: expected exit 0, got $run_exit (see events above)"
printf '%s' "$EVENTS" | jq -se '.' >/dev/null 2>&1 || fatal "events.ndjson is not valid NDJSON"
printf '%s' "$EVENTS" | jq -se 'any(.[]; .kind=="result" and .status=="succeeded")' >/dev/null \
    || fatal "events.ndjson has no succeeded result line"
[[ -n "$SUMMARY" ]] || fatal "summary.md is empty on success"

log "checking the patch round-tripped (README.md changed)"
PATCH="$(ssh_key "cd /workspace/repo && git add -A && git diff --cached $BASE_SHA")"
printf '%s' "$PATCH" | grep -q 'rooms-was-here' \
    || fatal "result patch does not contain the requested line; diff:\n$PATCH"
log "patch contains the requested line — success path OK"

log "auth-failure path (no CURSOR_API_KEY forwarded)"
ssh_nokey 'rm -f /workspace/out/events.ndjson /workspace/out/summary.md'
nokey_exit=0
ssh_nokey "node $RUNNER_JS < /dev/null" || nokey_exit=$?
NOKEY_EVENTS="$(ssh_nokey 'cat /workspace/out/events.ndjson 2>/dev/null || true')"
log "auth-failure events.ndjson:"
printf '%s\n' "$NOKEY_EVENTS"
[[ "$nokey_exit" -eq 2 ]] || fatal "auth-failure: expected exit 2, got $nokey_exit"
printf '%s' "$NOKEY_EVENTS" | jq -se 'any(.[]; .kind=="error" and .phase=="api_key")' >/dev/null \
    || fatal "auth-failure: no { kind:error, phase:api_key } line in events.ndjson"
log "auth-failure path OK (exit 2, api_key error)"

log "cursor runner dogfood passed"
