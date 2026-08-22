#!/usr/bin/env bash
# Reproducible Rooms-owned Phase-2 snapshot/fork killer subgate.
#
# This gate is intentionally hermetic and fail-closed:
# - it builds a fresh rootfs and rooms binary under a unique proof root;
# - it gives rooms a unique HOME/state tree, so the canonical slot reservation
#   and existing images are never mutated;
# - it records every created room id and only asks rooms to reap those ids;
# - it preserves the complete proof root on success and failure;
# - it emits summary.json as the machine-readable terminal result.
#
# Run as the ordinary rooms-host operator (not root). The script uses sudo -n
# only for the privileged host operations that rooms/Firecracker require.

set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly SCRIPT_DIR
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
readonly REPO_ROOT
readonly OPERATOR_HOME="${HOME:?HOME must be set}"
readonly PROOF_PARENT="$OPERATOR_HOME/.r2"

mkdir -p "$PROOF_PARENT"
PROOF_ROOT="$(mktemp -d "$PROOF_PARENT/XXXX")"
readonly PROOF_ROOT
# HOME is the proof root itself to keep jailed AF_UNIX paths below SUN_LEN.
readonly PROOF_HOME="$PROOF_ROOT"
readonly IMAGE_DIR="$PROOF_ROOT/images"
readonly IMAGE="$IMAGE_DIR/rootfs.ext4"
readonly SNAPSHOT_DIR="$PROOF_ROOT/snapshot"
readonly TARGET_DIR="$PROOF_ROOT/target"
readonly ARTIFACT_DIR="$PROOF_ROOT/artifacts"
readonly LOG_DIR="$PROOF_ROOT/logs"
readonly STATE_DIR="$PROOF_HOME/.local/state/rooms"
readonly CREATED_IDS_FILE="$PROOF_ROOT/created-room-ids.txt"
readonly FAILURES_FILE="$PROOF_ROOT/failures.ndjson"
readonly SUMMARY_FILE="$PROOF_ROOT/summary.json"
readonly CLEANUP_LOG="$LOG_DIR/cleanup.log"
readonly FLAT_RESTORE_JSON="$PROOF_ROOT/flat-restore.json"
readonly FLAT_RESTORE_OUT="$ARTIFACT_DIR/flat-restore"
readonly FLAT_RESTORE_AUDIT="$PROOF_ROOT/flat-restore-resource-audit.tsv"
readonly CANONICAL_IMAGES="$OPERATOR_HOME/rooms/images"
readonly CANONICAL_SLOT="$OPERATOR_HOME/.local/state/rooms/slots/1"
readonly SOURCE_MANIFEST_BEFORE="$PROOF_ROOT/source-files.before.sha256"
readonly SOURCE_MANIFEST_AFTER_BUILD="$PROOF_ROOT/source-files.after-build.sha256"
readonly SOURCE_MANIFEST_FINAL="$PROOF_ROOT/source-files.final.sha256"
readonly PROTECTED_BEFORE="$PROOF_ROOT/protected-canonical.before.tsv"
readonly PROTECTED_AFTER="$PROOF_ROOT/protected-canonical.after.tsv"
readonly SUN_LEN_BYTES=108
readonly WORST_CASE_ROOM_ID="00000000000000000000000000"
readonly WORST_CASE_UDS_PATH="$STATE_DIR/jailer/firecracker/$WORST_CASE_ROOM_ID/root/v.sock_5003"
readonly LITERAL_LATENCY_REQUIREMENT="<1s"
readonly WORKLOAD_SCOPE_NOTE="broadcast git-fsck commands prove eight parallel real repo workloads; this gate does not claim /work-driver consumer integration"
readonly RNG_SCOPE_NOTE="the retained static receiver has no stack canary or userspace DRBG; it reads ENTROPY first and forces RNDRESEEDCRNG before every post-resume fork/exec; kernel draws plus newly spawned ssh-keygen and sshd keys prove downstream divergence"
PROOF_TAG="$(basename "$PROOF_ROOT")"
readonly PROOF_TAG
readonly ENDPOINT_IF="r2p$PROOF_TAG"
readonly ENDPOINT_IP="198.18.255.254"
readonly ENDPOINT_ALIAS="rooms-phase2-$PROOF_TAG"
readonly ENDPOINT_COMMENT="rooms-p2-$PROOF_TAG"
readonly ENDPOINT_PORT_FILE="$PROOF_ROOT/endpoint.port"
readonly ENDPOINT_REQUESTS="$PROOF_ROOT/endpoint-requests.ndjson"
readonly SSH_WALL_TIMEOUT_SECONDS=20

export PATH="$OPERATOR_HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$PATH"

mkdir -p "$PROOF_HOME/.ssh" "$IMAGE_DIR" "$TARGET_DIR" "$ARTIFACT_DIR" "$LOG_DIR"
: >"$CREATED_IDS_FILE"
: >"$FAILURES_FILE"
: >"$CLEANUP_LOG"
umask 077

PHASE="initializing"
IN_EXIT=0
CLEANUP_OK="false"
ROOMS_SUBGATE_COMPLETED="false"
FULL_PHASE2_GATE_COMPLETED="false"
SOURCE_HEAD=""
SOURCE_MANIFEST_SHA256=""
BASE_REPO_SHA=""
ROOTFS_SHA256=""
SOURCE_AGENT_SHA256=""
IMAGE_AGENT_SHA256=""
SOURCE_APPLY_SHA256=""
IMAGE_APPLY_SHA256=""
PROOF_KEY_FINGERPRINT=""
SNAPSHOT_ID=""
SNAPSHOT_GUEST=""
SNAPSHOT_MEM_BYTES=""
FLAT_RESTORE_ID=""
SINGLE_PSS_KB=""
SINGLE_PSS_ANON_KB=""
SINGLE_PSS_FILE_KB=""
SINGLE_PRIVATE_DIRTY_KB=""
SINGLE_SHARED_CLEAN_KB=""
FLEET_PSS_KB=""
FLEET_PSS_ANON_KB=""
FLEET_PSS_FILE_KB=""
FLEET_PRIVATE_DIRTY_KB=""
FLEET_SHARED_CLEAN_KB=""
PSS_SHARING_RATIO=""
FLEET_ELAPSED_NS=""
WORST_CASE_UDS_BYTES=""
SNAPSHOT_MEM_INODE=""
SNAPSHOT_ATTESTATION_SHA256=""
RESUME_SECRET_VALUE=""
ENDPOINT_TOKEN=""
ENDPOINT_PORT=""
ENDPOINT_PID=""
ENDPOINT_PID_STARTTIME=""
ENDPOINT_PROCESS_STARTED="false"
ENDPOINT_INTERFACE_CREATED="false"
ENDPOINT_IFINDEX=""
ENDPOINT_ADDRESS=""
ENDPOINT_INTERFACE_STAGE="none"
ENDPOINT_RULE_INSTALLED="false"
LITERAL_LATENCY_PASS="false"
IMAGE_VERIFICATION_PASS="false"
SNAPSHOT_PASS="false"
FLAT_RESTORE_COMMAND_PASS="false"
FLAT_RESTORE_LIVE_FLAT_PASS="false"
FLAT_RESTORE_RESERVATION_PASS="false"
FLAT_RESTORE_CLEANUP_PASS="false"
FLAT_RESTORE_PASS="false"
PSS_PASS="false"
READINESS_PASS="false"
TOPOLOGY_PASS="false"
IDENTITY_PASS="false"
ISOLATION_PASS="false"
TWO_HOP_PASS="false"
WITNESS_PASS="false"
FINAL_LEAK_AUDIT_PASS="false"
HARD_FAILURES=0
PERFORMANCE_FAILURES=0
ROOMS_BIN=""

log() {
    printf '[phase2-killer] %s\n' "$*" >&2
}

witness_port_for_room() {
    local checksum

    checksum="$(printf '%s' "$1" | cksum | awk '{print $1}')" || return 1
    [[ "$checksum" =~ ^[0-9]+$ ]] || return 1
    printf '%s\n' "$((1024 + checksum % 50000))"
}

record_failure() {
    local severity="$1"
    local code="$2"
    local message="$3"
    local record
    record="$(jq -cn \
        --arg severity "$severity" \
        --arg code "$code" \
        --arg phase "$PHASE" \
        --arg message "$message" \
        '{severity:$severity, code:$code, phase:$phase, message:$message}')"
    printf '%s\n' "$record" >>"$FAILURES_FILE"
}

hard_failure() {
    local code="$1"
    local message="$2"
    HARD_FAILURES=$((HARD_FAILURES + 1))
    record_failure "hard" "$code" "$message"
    log "HARD FAIL [$code] $message"
}

performance_failure() {
    local code="$1"
    local message="$2"
    PERFORMANCE_FAILURES=$((PERFORMANCE_FAILURES + 1))
    record_failure "performance" "$code" "$message"
    log "PERFORMANCE FAIL [$code] $message"
}

fatal() {
    local code="$1"
    local message="$2"
    record_failure "fatal" "$code" "$message"
    log "FATAL [$code] $message"
    exit 1
}

on_error() {
    local status="$1"
    local line="$2"
    local command="$3"
    if ((IN_EXIT == 0)); then
        set +e
        record_failure \
            "fatal" \
            "unexpected_command_failure" \
            "line $line exited $status: $command"
        set -e
    fi
}

valid_room_id() {
    [[ "$1" =~ ^[0-9a-z]{26}$ ]]
}

add_created_id() {
    local room_id="$1"
    if ! valid_room_id "$room_id"; then
        return 1
    fi
    if grep -Fxq "$room_id" "$CREATED_IDS_FILE"; then
        return 0
    fi
    printf '%s\n' "$room_id" >>"$CREATED_IDS_FILE"
}

track_json_ids() {
    local json_file="$1"
    while read -r room_id; do
        add_created_id "$room_id" || fatal \
            "invalid_created_room_id" \
            "invalid room id in $json_file: $room_id"
    done < <(jq -er '.clones[].room_id' "$json_file")
}

run_rooms() {
    if [[ ! -x "$ROOMS_BIN" ]]; then
        return 127
    fi
    if [[ -v ROOMS_PHASE2_PROBE ]]; then
        sudo -n env \
            HOME="$PROOF_HOME" \
            PATH="$OPERATOR_HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
            ROOMS_PHASE2_PROBE="$ROOMS_PHASE2_PROBE" \
            "$ROOMS_BIN" "$@"
        return
    fi
    sudo -n env \
        HOME="$PROOF_HOME" \
        PATH="$OPERATOR_HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
        "$ROOMS_BIN" "$@"
}

# rooms runs as root and deliberately publishes state and immutable snapshots
# that the ordinary proof operator cannot traverse. Keep every inspection of
# that security boundary privileged too; an unprivileged test would otherwise
# confuse EACCES with absence and make both the evidence and leak audit lie.
privileged_paths_absent() {
    local state

    state="$(sudo -n sh -c '
        for path do
            if [ -e "$path" ] || [ -L "$path" ]; then
                printf present
                exit 0
            fi
        done
        printf absent
    ' sh "$@")" || return 1
    [[ "$state" == "absent" ]]
}

privileged_regular_file() {
    local state

    state="$(sudo -n sh -c '
        if [ -f "$1" ] && [ ! -L "$1" ]; then
            printf regular
        else
            printf other
        fi
    ' sh "$1")" || return 1
    [[ "$state" == "regular" ]]
}

privileged_directory() {
    local state

    state="$(sudo -n sh -c '
        if [ -d "$1" ] && [ ! -L "$1" ]; then
            printf directory
        else
            printf other
        fi
    ' sh "$1")" || return 1
    [[ "$state" == "directory" ]]
}

privileged_nonempty_file() {
    local state

    state="$(sudo -n sh -c '
        if [ -f "$1" ] && [ -s "$1" ] && [ ! -L "$1" ]; then
            printf nonempty
        else
            printf other
        fi
    ' sh "$1")" || return 1
    [[ "$state" == "nonempty" ]]
}

privileged_read_file() {
    sudo -n cat -- "$1"
}

capture_protected_entry() {
    local output="$1"
    local path="$2"
    if [[ -L "$path" ]]; then
        printf 'symlink\t%s\t%s\t%s\n' \
            "$path" \
            "$(stat -c '%d:%i:%s:%Y:%a:%u:%g' "$path")" \
            "$(readlink "$path")" \
            >>"$output"
        return
    fi
    if [[ -f "$path" ]]; then
        printf 'file\t%s\t%s\t%s\n' \
            "$path" \
            "$(stat -c '%d:%i:%s:%Y:%a:%u:%g' "$path")" \
            "$(sha256sum "$path" | awk '{print $1}')" \
            >>"$output"
        return
    fi
    if [[ -d "$path" ]]; then
        printf 'dir\t%s\t%s\n' \
            "$path" \
            "$(stat -c '%d:%i:%s:%Y:%a:%u:%g' "$path")" \
            >>"$output"
        return
    fi
    if [[ -e "$path" ]]; then
        printf 'other\t%s\t%s\n' \
            "$path" \
            "$(stat -c '%d:%i:%s:%Y:%a:%u:%g' "$path")" \
            >>"$output"
        return
    fi
    printf 'absent\t%s\n' "$path" >>"$output"
}

capture_protected_state() {
    local output="$1"
    local path
    : >"$output"
    capture_protected_entry "$output" "$CANONICAL_SLOT"
    if [[ ! -d "$CANONICAL_IMAGES" ]]; then
        printf 'absent\t%s\n' "$CANONICAL_IMAGES" >>"$output"
        return
    fi
    capture_protected_entry "$output" "$CANONICAL_IMAGES"
    while IFS= read -r -d '' path; do
        capture_protected_entry "$output" "$path"
    done < <(find "$CANONICAL_IMAGES" -mindepth 1 -print0 | sort -z)
}

source_manifest() {
    local output="$1"
    (
        cd "$REPO_ROOT"
        git ls-files -co --exclude-standard -z \
            | sort -z \
            | xargs -0 -r sha256sum
    ) >"$output"
}

endpoint_pid_is_owned() {
    local current_cmdline
    local current_stat
    local current_starttime
    local confirmed_stat
    local confirmed_starttime

    [[ "$ENDPOINT_PID" =~ ^[0-9]+$ ]] || return 1
    [[ "$ENDPOINT_PID_STARTTIME" =~ ^[0-9]+$ ]] || return 1
    current_stat="$(cat "/proc/$ENDPOINT_PID/stat" 2>/dev/null)" || return 1
    current_starttime="$(awk '{print $22}' <<<"$current_stat")" || return 1
    [[ "$current_starttime" == "$ENDPOINT_PID_STARTTIME" ]] || return 1
    current_cmdline="$(tr '\0' '\n' 2>/dev/null <"/proc/$ENDPOINT_PID/cmdline")" \
        || return 1
    confirmed_stat="$(cat "/proc/$ENDPOINT_PID/stat" 2>/dev/null)" || return 1
    confirmed_starttime="$(awk '{print $22}' <<<"$confirmed_stat")" || return 1
    [[ "$confirmed_starttime" == "$ENDPOINT_PID_STARTTIME" ]] || return 1
    grep -Fxq "$ENDPOINT_REQUESTS" <<<"$current_cmdline"
}

endpoint_interface_is_owned() {
    local current_address
    local current_alias
    local current_ifindex

    [[ "$ENDPOINT_IFINDEX" =~ ^[0-9]+$ ]] || return 1
    [[ "$ENDPOINT_ADDRESS" =~ ^02(:[[:xdigit:]]{2}){5}$ ]] || return 1
    [[ "$ENDPOINT_INTERFACE_STAGE" == "fingerprinted" \
        || "$ENDPOINT_INTERFACE_STAGE" == "alias_confirmed" ]] || return 1
    sudo -n ip link show "$ENDPOINT_IF" >/dev/null 2>&1 || return 1
    current_ifindex="$(sudo -n cat "/sys/class/net/$ENDPOINT_IF/ifindex")" || return 1
    current_address="$(sudo -n cat "/sys/class/net/$ENDPOINT_IF/address")" || return 1
    current_alias="$(sudo -n cat "/sys/class/net/$ENDPOINT_IF/ifalias")" || return 1
    [[ "$current_ifindex" == "$ENDPOINT_IFINDEX" \
        && "$current_address" == "$ENDPOINT_ADDRESS" ]] || return 1
    if [[ "$ENDPOINT_INTERFACE_STAGE" == "alias_confirmed" ]]; then
        [[ "$current_alias" == "$ENDPOINT_ALIAS" ]]
        return
    fi
    [[ -z "$current_alias" || "$current_alias" == "$ENDPOINT_ALIAS" ]]
}

endpoint_rule_present() {
    sudo -n iptables -C INPUT \
        -i 'veth-h+' \
        -d "$ENDPOINT_IP/32" \
        -p tcp \
        --dport "$ENDPOINT_PORT" \
        -m comment \
        --comment "$ENDPOINT_COMMENT" \
        -j ACCEPT \
        >/dev/null 2>&1
}

endpoint_absent() {
    local address_dump
    local failed=0
    local input_dump
    local link_dump

    link_dump="$(sudo -n ip -o link show)" || return 1
    address_dump="$(sudo -n ip -4 -o addr show)" || return 1
    input_dump="$(sudo -n iptables -S INPUT)" || return 1
    if awk -F': ' '{sub(/@.*/, "", $2); print $2}' <<<"$link_dump" \
        | grep -Fxq "$ENDPOINT_IF"; then
        failed=1
    fi
    if awk -v address="$ENDPOINT_IP/32" '$4 == address {found=1} END {exit !found}' \
        <<<"$address_dump"; then
        failed=1
    fi
    if [[ -n "$ENDPOINT_PORT" ]] && endpoint_rule_present; then
        failed=1
    fi
    if grep -Fq -- "$ENDPOINT_COMMENT" <<<"$input_dump"; then
        failed=1
    fi
    if endpoint_pid_is_owned; then
        failed=1
    fi

    ((failed == 0))
}

cleanup_proof_endpoint() {
    local failed=0
    local attempt
    local interface_present="false"
    local rule_present="false"

    if [[ -n "$ENDPOINT_PORT" ]] && endpoint_rule_present; then
        rule_present="true"
    fi
    if [[ "$ENDPOINT_RULE_INSTALLED" == "true" && "$rule_present" != "true" ]]; then
        failed=1
    fi
    if [[ "$rule_present" == "true" ]]; then
        sudo -n iptables -D INPUT \
            -i 'veth-h+' \
            -d "$ENDPOINT_IP/32" \
            -p tcp \
            --dport "$ENDPOINT_PORT" \
            -m comment \
            --comment "$ENDPOINT_COMMENT" \
            -j ACCEPT \
            >>"$CLEANUP_LOG" 2>&1 || failed=1
    fi
    if [[ -n "$ENDPOINT_PORT" ]] && endpoint_rule_present; then
        failed=1
    else
        ENDPOINT_RULE_INSTALLED="false"
    fi

    if [[ "$ENDPOINT_PROCESS_STARTED" == "true" ]]; then
        if endpoint_pid_is_owned; then
            kill -TERM "$ENDPOINT_PID" >>"$CLEANUP_LOG" 2>&1 || failed=1
        elif kill -0 "$ENDPOINT_PID" 2>/dev/null; then
            # The saved PID now belongs to a foreign process. Never signal it.
            failed=1
        fi
        for attempt in $(seq 1 20); do
            endpoint_pid_is_owned || break
            sleep 0.05
        done
        if endpoint_pid_is_owned; then
            kill -KILL "$ENDPOINT_PID" >>"$CLEANUP_LOG" 2>&1 || failed=1
        fi
        # Waiting is harmless even if the numeric PID was recycled: Bash waits
        # on its child job table, not an arbitrary same-numbered process.
        wait "$ENDPOINT_PID" 2>>"$CLEANUP_LOG" || true
        ENDPOINT_PROCESS_STARTED="false"
    fi
    if endpoint_pid_is_owned; then
        failed=1
    fi

    if sudo -n ip link show "$ENDPOINT_IF" >/dev/null 2>&1; then
        interface_present="true"
    fi
    if [[ "$ENDPOINT_INTERFACE_CREATED" == "true" && "$interface_present" != "true" ]]; then
        failed=1
    fi
    if [[ "$interface_present" == "true" ]]; then
        if [[ "$ENDPOINT_INTERFACE_CREATED" == "true" ]] \
            && endpoint_interface_is_owned; then
            sudo -n ip link delete "$ENDPOINT_IF" \
                >>"$CLEANUP_LOG" 2>&1 || failed=1
        else
            # A same-name interface with different custody is foreign.
            failed=1
        fi
    fi
    if sudo -n ip link show "$ENDPOINT_IF" >/dev/null 2>&1; then
        failed=1
    else
        ENDPOINT_INTERFACE_CREATED="false"
        ENDPOINT_INTERFACE_STAGE="none"
        ENDPOINT_IFINDEX=""
        ENDPOINT_ADDRESS=""
    fi

    endpoint_absent || failed=1
    ((failed == 0))
}

start_proof_endpoint() {
    local attempt
    local address_hash

    endpoint_absent || return 1
    ENDPOINT_TOKEN="$(printf '%s' "$SOURCE_HEAD:$PROOF_TAG" | sha256sum | awk '{print $1}')" \
        || return 1
    : >"$ENDPOINT_REQUESTS" || return 1
    rm -f "$ENDPOINT_PORT_FILE" || return 1

    address_hash="$(printf '%s' "$PROOF_TAG" | sha256sum | awk '{print $1}')" || return 1
    ENDPOINT_ADDRESS="02:${address_hash:0:2}:${address_hash:2:2}:${address_hash:4:2}:${address_hash:6:2}:${address_hash:8:2}"
    sudo -n ip link add name "$ENDPOINT_IF" address "$ENDPOINT_ADDRESS" type dummy || return 1
    ENDPOINT_INTERFACE_CREATED="true"
    ENDPOINT_IFINDEX="$(sudo -n cat "/sys/class/net/$ENDPOINT_IF/ifindex")" || return 1
    [[ "$ENDPOINT_IFINDEX" =~ ^[0-9]+$ ]] || return 1
    ENDPOINT_INTERFACE_STAGE="fingerprinted"
    sudo -n ip link set dev "$ENDPOINT_IF" alias "$ENDPOINT_ALIAS" || return 1
    sudo -n ip addr add "$ENDPOINT_IP/32" dev "$ENDPOINT_IF" || return 1
    sudo -n ip link set dev "$ENDPOINT_IF" up || return 1
    [[ "$(sudo -n cat "/sys/class/net/$ENDPOINT_IF/ifalias")" == "$ENDPOINT_ALIAS" ]] \
        || return 1
    ENDPOINT_INTERFACE_STAGE="alias_confirmed"
    sudo -n ip -4 -o addr show dev "$ENDPOINT_IF" \
        | awk -v address="$ENDPOINT_IP/32" \
            '$4 == address {found=1} END {exit !found}' \
        || return 1

    python3 -u - \
        "$ENDPOINT_IP" \
        "$ENDPOINT_PORT_FILE" \
        "$ENDPOINT_REQUESTS" \
        "$ENDPOINT_TOKEN" \
        >"$LOG_DIR/endpoint.stdout" \
        2>"$LOG_DIR/endpoint.stderr" <<'PY' &
import http.server
import json
import os
import sys
import threading

listen_ip, port_path, requests_path, token = sys.argv[1:]
lock = threading.Lock()


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        room_id = self.path.split("?", 1)[0].lstrip("/")
        record = {"path": self.path, "peer": self.client_address[0]}
        with lock:
            with open(requests_path, "a", encoding="utf-8") as stream:
                stream.write(json.dumps(record, sort_keys=True) + "\n")
        body = f"{token}:{room_id}\n".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        return


server = http.server.ThreadingHTTPServer((listen_ip, 0), Handler)
port_tmp = f"{port_path}.tmp.{os.getpid()}"
with open(port_tmp, "w", encoding="ascii") as stream:
    stream.write(f"{server.server_port}\n")
os.replace(port_tmp, port_path)
server.serve_forever()
PY
    ENDPOINT_PID="$!"
    ENDPOINT_PROCESS_STARTED="true"
    ENDPOINT_PID_STARTTIME="$(awk '{print $22}' "/proc/$ENDPOINT_PID/stat")" || return 1
    [[ "$ENDPOINT_PID_STARTTIME" =~ ^[0-9]+$ ]] || return 1

    for attempt in $(seq 1 100); do
        if [[ -s "$ENDPOINT_PORT_FILE" ]] && endpoint_pid_is_owned; then
            break
        fi
        kill -0 "$ENDPOINT_PID" 2>/dev/null || return 1
        sleep 0.05 || return 1
    done
    [[ -s "$ENDPOINT_PORT_FILE" ]] && endpoint_pid_is_owned || return 1
    ENDPOINT_PORT="$(<"$ENDPOINT_PORT_FILE")"
    [[ "$ENDPOINT_PORT" =~ ^[0-9]+$ ]] \
        && ((ENDPOINT_PORT >= 1024 && ENDPOINT_PORT <= 65535)) \
        || return 1

    sudo -n iptables -I INPUT 1 \
        -i 'veth-h+' \
        -d "$ENDPOINT_IP/32" \
        -p tcp \
        --dport "$ENDPOINT_PORT" \
        -m comment \
        --comment "$ENDPOINT_COMMENT" \
        -j ACCEPT || return 1
    ENDPOINT_RULE_INSTALLED="true"
    endpoint_rule_present
}

discover_proof_room_ids() {
    local listing
    local path
    local owner

    if listing="$(run_rooms ls --json 2>>"$CLEANUP_LOG")"; then
        while read -r owner; do
            add_created_id "$owner" || true
        done < <(jq -r '.rooms[].id' <<<"$listing" 2>>"$CLEANUP_LOG")
    fi

    if ! privileged_paths_absent "$STATE_DIR"; then
        privileged_directory "$STATE_DIR" || return 1
        listing="$(sudo -n find "$STATE_DIR" \
            -mindepth 1 -maxdepth 1 -type d -print \
            2>>"$CLEANUP_LOG")" || return 1
        while read -r path; do
            [[ -n "$path" ]] || continue
            owner="$(basename "$path")"
            add_created_id "$owner" || true
        done <<<"$listing"
    fi

    if ! privileged_paths_absent "$STATE_DIR/restore-intents"; then
        privileged_directory "$STATE_DIR/restore-intents" || return 1
        listing="$(sudo -n find "$STATE_DIR/restore-intents" \
            -maxdepth 1 -type f -name '*.json' -print \
            2>>"$CLEANUP_LOG")" || return 1
        while read -r path; do
            [[ -n "$path" ]] || continue
            owner="$(basename "$path" .json)"
            add_created_id "$owner" || true
        done <<<"$listing"
    fi

    if ! privileged_paths_absent "$STATE_DIR/snapshot-intents"; then
        privileged_directory "$STATE_DIR/snapshot-intents" || return 1
        listing="$(sudo -n find "$STATE_DIR/snapshot-intents" \
            -maxdepth 1 -type f -name '*.json' -print \
            2>>"$CLEANUP_LOG")" || return 1
        while read -r path; do
            [[ -n "$path" ]] || continue
            owner="$(basename "$path" .json)"
            add_created_id "$owner" || true
        done <<<"$listing"
    fi

    if ! privileged_paths_absent "$STATE_DIR/clonenets"; then
        privileged_directory "$STATE_DIR/clonenets" || return 1
        listing="$(sudo -n find "$STATE_DIR/clonenets" \
            -maxdepth 1 -type f -print \
            2>>"$CLEANUP_LOG")" || return 1
        while read -r path; do
            [[ -n "$path" ]] || continue
            owner="$(sudo -n sed -n '1p' "$path" 2>>"$CLEANUP_LOG")" || return 1
            add_created_id "$owner" || true
        done <<<"$listing"
    fi

    if ! privileged_paths_absent "$STATE_DIR/slots/1"; then
        privileged_regular_file "$STATE_DIR/slots/1" || return 1
        owner="$(privileged_read_file "$STATE_DIR/slots/1")" || return 1
        add_created_id "$owner" || true
    fi
}

discover_proof_json_ids() {
    local json_file
    local room_id
    for json_file in \
        "$PROOF_ROOT/base.json" \
        "$FLAT_RESTORE_JSON" \
        "$PROOF_ROOT/single.json" \
        "$PROOF_ROOT/fleet.json" \
        "$PROOF_ROOT/witness-batch.json"; do
        [[ -s "$json_file" ]] || continue
        while read -r room_id; do
            add_created_id "$room_id" || true
        done < <(
            jq -r \
                '[.room_id?, .clones[]?.room_id?, .failures[]?.room_id?]
                 | .[] | select(type == "string")' \
                "$json_file" 2>>"$CLEANUP_LOG" || true
        )
    done
}

recover_proof_snapshots() {
    local report
    local snapshot_id
    if ! report="$(run_rooms snapshot-recover --json 2>>"$CLEANUP_LOG")"; then
        return 1
    fi
    while read -r snapshot_id; do
        run_rooms snapshot-recover "$snapshot_id" --json \
            >>"$CLEANUP_LOG" 2>&1 || return 1
    done < <(jq -r '.pending[].snapshot_id' <<<"$report" 2>>"$CLEANUP_LOG")
}

locked_terminal_slot_record() {
    local lock_path="$STATE_DIR/slots.lock"
    local slot_path="$STATE_DIR/slots/1"

    privileged_regular_file "$lock_path" || return 20
    sudo -n flock --shared --wait 5 "$lock_path" sh -c '
        path=$1
        [ -f "$path" ] && [ ! -L "$path" ] || exit 21
        digest="$(sha256sum "$path")" || exit 22
        digest=${digest%% *}
        details="$(stat -c "dev=%d inode=%i size=%s mtime=%Y mode=%a uid=%u gid=%g" "$path")" \
            || exit 23
        token="$(cat -- "$path")" || exit 24
        printf "%s\n%s\n%s\n" "$digest" "$details" "$token"
    ' sh "$slot_path"
}

log_terminal_slot_mismatch() {
    local reason="$1"
    local expected="$2"
    local observed="<unreadable>"
    local digest="<unavailable>"
    local details="<unavailable>"
    local expected_quoted
    local observed_quoted
    local details_quoted

    if (( $# >= 5 )); then
        observed="$3"
        digest="$4"
        details="$5"
    elif privileged_paths_absent "$STATE_DIR/slots/1"; then
        observed="<absent>"
        digest="<absent>"
        details="<absent>"
    else
        details="$(sudo -n stat -c \
            'type=%F dev=%d inode=%i size=%s mtime=%Y mode=%a uid=%u gid=%g' \
            "$STATE_DIR/slots/1" 2>>"$CLEANUP_LOG")" || details="<stat-failed>"
        if privileged_regular_file "$STATE_DIR/slots/1"; then
            observed="$(privileged_read_file "$STATE_DIR/slots/1" \
                2>>"$CLEANUP_LOG")" || observed="<read-failed>"
            digest="$(sudo -n sha256sum "$STATE_DIR/slots/1" \
                2>>"$CLEANUP_LOG" | awk '{print $1}')" || digest="<digest-failed>"
        elif sudo -n test -L "$STATE_DIR/slots/1"; then
            observed="<symlink:$(sudo -n readlink "$STATE_DIR/slots/1" \
                2>>"$CLEANUP_LOG" || printf 'readlink-failed')>"
            digest="<not-regular>"
        else
            observed="<not-regular>"
            digest="<not-regular>"
        fi
    fi
    printf -v expected_quoted '%q' "$expected"
    printf -v observed_quoted '%q' "$observed"
    printf -v details_quoted '%q' "$details"
    printf 'terminal proof slot mismatch: reason=%s expected=%s observed=%s sha256=%s stat=%s\n' \
        "$reason" "$expected_quoted" "$observed_quoted" "$digest" "$details_quoted" \
        >>"$CLEANUP_LOG"
}

assert_terminal_proof_slot() {
    local actual_digest
    local expected="@reservation $SNAPSHOT_ID"
    local expected_digest="${RESERVATION_SHA256:-}"
    local recovered_snapshot_id
    local record
    local record_status=0
    local record_tail
    local slot_details
    local token

    if privileged_paths_absent "$STATE_DIR/slots/1"; then
        if [[ -z "$SNAPSHOT_ID" ]]; then
            return 0
        fi
        log_terminal_slot_mismatch "slot-absent" "$expected"
        return 1
    fi
    record="$(locked_terminal_slot_record)" || record_status=$?
    if ((record_status != 0)); then
        log_terminal_slot_mismatch "locked-read-exit-$record_status" "$expected"
        return 1
    fi
    if [[ "$record" != *$'\n'* ]]; then
        log_terminal_slot_mismatch \
            "malformed-locked-record" "$expected" "<unparseable>" \
            "<unparseable>" "<unparseable>"
        return 1
    fi
    actual_digest="${record%%$'\n'*}"
    record_tail="${record#*$'\n'}"
    if [[ "$record_tail" == *$'\n'* ]]; then
        slot_details="${record_tail%%$'\n'*}"
        token="${record_tail#*$'\n'}"
    else
        # Command substitution removes trailing newlines, so an empty slot
        # body leaves only the digest and stat lines in the record.
        slot_details="$record_tail"
        token=""
    fi
    if [[ ! "$actual_digest" =~ ^[[:xdigit:]]{64}$ || -z "$slot_details" ]]; then
        log_terminal_slot_mismatch \
            "invalid-locked-record" "$expected" "$token" "$actual_digest" "$slot_details"
        return 1
    fi
    if [[ -n "$SNAPSHOT_ID" ]]; then
        if [[ "$token" == "$expected" \
            && ( -z "$expected_digest" || "$actual_digest" == "$expected_digest" ) ]]; then
            return 0
        fi
        log_terminal_slot_mismatch \
            "reservation-token-or-digest" "$expected" "$token" "$actual_digest" "$slot_details"
        return 1
    fi
    if [[ "$token" =~ ^@reservation\ ([0-9a-z]{26})$ ]]; then
        recovered_snapshot_id="${BASH_REMATCH[1]}"
        if privileged_nonempty_file "$SNAPSHOT_DIR/snapshot.json" \
            && sudo -n jq -e --arg snapshot_id "$recovered_snapshot_id" \
                '.snapshot_id == $snapshot_id' \
                "$SNAPSHOT_DIR/snapshot.json" >/dev/null 2>>"$CLEANUP_LOG"; then
            SNAPSHOT_ID="$recovered_snapshot_id"
            return 0
        fi
    fi
    log_terminal_slot_mismatch \
        "reservation-not-recoverable" "$expected" "$token" "$actual_digest" "$slot_details"
    return 1
}

proof_transients_absent() {
    local directory
    local found
    for directory in restore-intents snapshot-intents clonenets; do
        if privileged_paths_absent "$STATE_DIR/$directory"; then
            continue
        fi
        privileged_directory "$STATE_DIR/$directory" || return 1
        if [[ "$directory" == "clonenets" ]]; then
            found="$(sudo -n find "$STATE_DIR/$directory" \
                -mindepth 1 -maxdepth 1 -print -quit)" \
                || return 1
        else
            # Snapshot transaction lock files are durable serialization
            # primitives, not live intents. Only the indexed JSON is custody.
            found="$(sudo -n find "$STATE_DIR/$directory" \
                -mindepth 1 -maxdepth 1 -name '*.json' -print -quit)" \
                || return 1
        fi
        if [[ -n "$found" ]]; then
            return 1
        fi
    done
    directory="$STATE_DIR/snapshot-attestations"
    if ! privileged_paths_absent "$directory"; then
        privileged_directory "$directory" || return 1
        found="$(sudo -n find "$directory" -mindepth 1 -maxdepth 1 \
            \( -name '.*.tmp' -o -name '.*.preflight' \) -print -quit)" \
            || return 1
        [[ -z "$found" ]] || return 1
    fi
    return 0
}

cleanup_created_rooms() {
    local room_id
    local failed=0
    local listing

    if ! cleanup_proof_endpoint; then
        printf '%s\n' 'cleanup check failed: proof endpoint' >>"$CLEANUP_LOG"
        failed=1
    fi

    if [[ ! -x "$ROOMS_BIN" ]]; then
        if ! assert_terminal_proof_slot; then
            printf '%s\n' 'cleanup check failed: terminal proof slot' >>"$CLEANUP_LOG"
            failed=1
        fi
        if ! proof_transients_absent; then
            printf '%s\n' 'cleanup check failed: proof transients' >>"$CLEANUP_LOG"
            failed=1
        fi
        if ((failed == 0)); then
            CLEANUP_OK="true"
            return 0
        fi
        CLEANUP_OK="false"
        return 1
    fi

    if ! recover_proof_snapshots; then
        printf '%s\n' 'cleanup check failed: initial snapshot recovery' >>"$CLEANUP_LOG"
        failed=1
    fi
    discover_proof_json_ids
    if ! discover_proof_room_ids; then
        printf '%s\n' 'cleanup check failed: initial room discovery' >>"$CLEANUP_LOG"
        failed=1
    fi

    while read -r room_id; do
        valid_room_id "$room_id" || continue
        # Both operations are idempotent cleanup attempts. A room may already
        # have reached terminal absence (for example snapshot consumes its
        # base, and command-mode clones reap themselves). The authoritative
        # result is the exact state/resource audit below, not an "already
        # gone" diagnostic from a redundant cleanup attempt.
        run_rooms kill "$room_id" --json \
            >>"$CLEANUP_LOG" 2>&1 || true
        run_rooms gc "$room_id" \
            >>"$CLEANUP_LOG" 2>&1 || true
    done < <(sort -u "$CREATED_IDS_FILE")

    # The global pass owns reconciliation of orphaned restore intents and
    # CloneNet allocator claims. It runs inside the hermetic proof HOME; the
    # exact terminal audits below still decide whether cleanup succeeded.
    run_rooms gc >>"$CLEANUP_LOG" 2>&1 || true

    if ! recover_proof_snapshots; then
        printf '%s\n' 'cleanup check failed: terminal snapshot recovery' >>"$CLEANUP_LOG"
        failed=1
    fi
    if ! discover_proof_room_ids; then
        printf '%s\n' 'cleanup check failed: terminal room discovery' >>"$CLEANUP_LOG"
        failed=1
    fi
    if listing="$(run_rooms ls --json 2>>"$CLEANUP_LOG")"; then
        if ! jq -e '.rooms | length == 0' >/dev/null <<<"$listing"; then
            printf '%s\n' 'cleanup check failed: nonempty room roster' >>"$CLEANUP_LOG"
            failed=1
        fi
    else
        printf '%s\n' 'cleanup check failed: unreadable room roster' >>"$CLEANUP_LOG"
        failed=1
    fi
    if ! assert_terminal_proof_slot; then
        printf '%s\n' 'cleanup check failed: terminal proof slot' >>"$CLEANUP_LOG"
        failed=1
    fi
    if ! proof_transients_absent; then
        printf '%s\n' 'cleanup check failed: proof transients' >>"$CLEANUP_LOG"
        failed=1
    fi
    if ! audit_global_clone_absence; then
        printf '%s\n' 'cleanup check failed: global clone resources' >>"$CLEANUP_LOG"
        failed=1
    fi
    if pgrep -x firecracker >/dev/null 2>&1 || pgrep -x jailer >/dev/null 2>&1; then
        printf '%s\n' 'cleanup check failed: VMM process remains' >>"$CLEANUP_LOG"
        failed=1
    fi
    if findmnt -rn -o TARGET | grep -Fq "$STATE_DIR/jailer/"; then
        printf '%s\n' 'cleanup check failed: proof jail mount remains' >>"$CLEANUP_LOG"
        failed=1
    fi

    if ((failed == 0)); then
        CLEANUP_OK="true"
        return 0
    fi
    CLEANUP_OK="false"
    return 1
}

write_summary() {
    local exit_code="$1"
    local status="failed"
    local created_ids
    local summary_tmp="$SUMMARY_FILE.tmp.$$"

    if ((exit_code == 0)); then
        status="partial"
    fi
    created_ids="$(sort -u "$CREATED_IDS_FILE" 2>/dev/null || true)"

    jq -n \
        --arg status "$status" \
        --argjson exit_code "$exit_code" \
        --arg phase "$PHASE" \
        --arg proof_root "$PROOF_ROOT" \
        --arg source_head "$SOURCE_HEAD" \
        --arg source_manifest_sha256 "$SOURCE_MANIFEST_SHA256" \
        --arg base_repo_sha "$BASE_REPO_SHA" \
        --arg rootfs_sha256 "$ROOTFS_SHA256" \
        --arg source_agent_sha256 "$SOURCE_AGENT_SHA256" \
        --arg image_agent_sha256 "$IMAGE_AGENT_SHA256" \
        --arg source_apply_sha256 "$SOURCE_APPLY_SHA256" \
        --arg image_apply_sha256 "$IMAGE_APPLY_SHA256" \
        --arg proof_key_fingerprint "$PROOF_KEY_FINGERPRINT" \
        --arg snapshot_id "$SNAPSHOT_ID" \
        --arg snapshot_guest "$SNAPSHOT_GUEST" \
        --arg snapshot_mem_bytes "$SNAPSHOT_MEM_BYTES" \
        --arg snapshot_mem_inode "$SNAPSHOT_MEM_INODE" \
        --arg snapshot_attestation_sha256 "$SNAPSHOT_ATTESTATION_SHA256" \
        --arg flat_restore_id "$FLAT_RESTORE_ID" \
        --arg flat_restore_command_pass "$FLAT_RESTORE_COMMAND_PASS" \
        --arg flat_restore_live_flat_pass "$FLAT_RESTORE_LIVE_FLAT_PASS" \
        --arg flat_restore_reservation_pass "$FLAT_RESTORE_RESERVATION_PASS" \
        --arg flat_restore_cleanup_pass "$FLAT_RESTORE_CLEANUP_PASS" \
        --arg flat_restore_pass "$FLAT_RESTORE_PASS" \
        --arg single_pss_kb "$SINGLE_PSS_KB" \
        --arg single_pss_anon_kb "$SINGLE_PSS_ANON_KB" \
        --arg single_pss_file_kb "$SINGLE_PSS_FILE_KB" \
        --arg single_private_dirty_kb "$SINGLE_PRIVATE_DIRTY_KB" \
        --arg single_shared_clean_kb "$SINGLE_SHARED_CLEAN_KB" \
        --arg fleet_pss_kb "$FLEET_PSS_KB" \
        --arg fleet_pss_anon_kb "$FLEET_PSS_ANON_KB" \
        --arg fleet_pss_file_kb "$FLEET_PSS_FILE_KB" \
        --arg fleet_private_dirty_kb "$FLEET_PRIVATE_DIRTY_KB" \
        --arg fleet_shared_clean_kb "$FLEET_SHARED_CLEAN_KB" \
        --arg pss_sharing_ratio "$PSS_SHARING_RATIO" \
        --arg fleet_elapsed_ns "$FLEET_ELAPSED_NS" \
        --arg worst_case_uds_path "$WORST_CASE_UDS_PATH" \
        --arg worst_case_uds_bytes "$WORST_CASE_UDS_BYTES" \
        --arg literal_latency_requirement "$LITERAL_LATENCY_REQUIREMENT" \
        --arg literal_latency_pass "$LITERAL_LATENCY_PASS" \
        --arg image_verification_pass "$IMAGE_VERIFICATION_PASS" \
        --arg snapshot_pass "$SNAPSHOT_PASS" \
        --arg pss_pass "$PSS_PASS" \
        --arg readiness_pass "$READINESS_PASS" \
        --arg topology_pass "$TOPOLOGY_PASS" \
        --arg identity_pass "$IDENTITY_PASS" \
        --arg isolation_pass "$ISOLATION_PASS" \
        --arg two_hop_pass "$TWO_HOP_PASS" \
        --arg witness_pass "$WITNESS_PASS" \
        --arg final_leak_audit_pass "$FINAL_LEAK_AUDIT_PASS" \
        --arg cleanup_ok "$CLEANUP_OK" \
        --arg rooms_subgate_completed "$ROOMS_SUBGATE_COMPLETED" \
        --arg full_phase2_gate_completed "$FULL_PHASE2_GATE_COMPLETED" \
        --arg workload_scope_note "$WORKLOAD_SCOPE_NOTE" \
        --arg rng_scope_note "$RNG_SCOPE_NOTE" \
        --arg endpoint_ip "$ENDPOINT_IP" \
        --arg endpoint_port "$ENDPOINT_PORT" \
        --arg created_ids "$created_ids" \
        --slurpfile failures "$FAILURES_FILE" \
        'def number_or_null($value):
             if $value == "" then null else ($value | tonumber) end;
         {
           schema_version: 2,
           status: $status,
           gate_scope: "rooms_snapshot_fleet_substrate",
           exit_code: $exit_code,
           terminal_phase: $phase,
           proof_root: $proof_root,
           source: {
             head: $source_head,
             manifest_sha256: $source_manifest_sha256,
             warmed_repo_head: $base_repo_sha
           },
           fresh_image: {
             verified: ($image_verification_pass == "true"),
             rootfs_sha256: $rootfs_sha256,
             source_resume_agent_sha256: $source_agent_sha256,
             image_resume_agent_sha256: $image_agent_sha256,
             source_native_apply_sha256: $source_apply_sha256,
             image_native_apply_sha256: $image_apply_sha256,
             proof_ssh_key_fingerprint: $proof_key_fingerprint
           },
           snapshot: {
             id: $snapshot_id,
             frozen_guest_ip: $snapshot_guest,
             memory_bytes: number_or_null($snapshot_mem_bytes),
             memory_dev_inode: $snapshot_mem_inode,
             local_attestation_sha256: $snapshot_attestation_sha256
           },
           flat_restore: {
             room_id: $flat_restore_id,
             command_succeeded: ($flat_restore_command_pass == "true"),
             live_flat_path: ($flat_restore_live_flat_pass == "true"),
             exact_reservation_returned: ($flat_restore_reservation_pass == "true"),
             cleanup_complete: ($flat_restore_cleanup_pass == "true"),
             passed: ($flat_restore_pass == "true")
           },
           performance: {
             fleet_ready_elapsed_ns: number_or_null($fleet_elapsed_ns),
             literal_requirement: $literal_latency_requirement,
             required_max_ns: 1000000000,
             literal_under_one_second: ($literal_latency_pass == "true"),
             single_clone_pss_kb: number_or_null($single_pss_kb),
             fleet_pss_kb: number_or_null($fleet_pss_kb),
             fleet_to_naive_pss_ratio: number_or_null($pss_sharing_ratio),
             memory_breakdown_kb: {
               single_clone: {
                 pss: number_or_null($single_pss_kb),
                 pss_anon: number_or_null($single_pss_anon_kb),
                 pss_file: number_or_null($single_pss_file_kb),
                 private_dirty: number_or_null($single_private_dirty_kb),
                 shared_clean: number_or_null($single_shared_clean_kb)
               },
               fleet: {
                 pss: number_or_null($fleet_pss_kb),
                 pss_anon: number_or_null($fleet_pss_anon_kb),
                 pss_file: number_or_null($fleet_pss_file_kb),
                 private_dirty: number_or_null($fleet_private_dirty_kb),
                 shared_clean: number_or_null($fleet_shared_clean_kb)
               }
             }
           },
           unix_socket_path_budget: {
             worst_case_path: $worst_case_uds_path,
             measured_bytes: number_or_null($worst_case_uds_bytes),
             sun_len_bytes: 108,
             passes: (number_or_null($worst_case_uds_bytes) != null and
                      number_or_null($worst_case_uds_bytes) < 108)
           },
           checks: {
             fresh_image_hash_and_agent: ($image_verification_pass == "true"),
             warm_neutral_snapshot: ($snapshot_pass == "true"),
             flat_restore_command_path: ($flat_restore_pass == "true"),
             readiness: ($readiness_pass == "true"),
             topology_and_shared_inode: ($topology_pass == "true"),
             pss_density: ($pss_pass == "true"),
             identity_clock_rng_host_keys: ($identity_pass == "true"),
             cross_clone_isolation: ($isolation_pass == "true"),
             two_hop_return_path: ($two_hop_pass == "true"),
             eight_witnesses: ($witness_pass == "true"),
             final_leak_audit: ($final_leak_audit_pass == "true"),
             cleanup: ($cleanup_ok == "true"),
             rooms_subgate_completed: ($rooms_subgate_completed == "true"),
             full_phase2_gate_completed: ($full_phase2_gate_completed == "true")
           },
           evidence: {
             provenance: "provenance.txt",
             build_hashes: "build-artifacts.sha256",
             native_resume_artifacts: "native-resume-artifacts.tsv",
             snapshot_hashes: "snapshot-artifacts.sha256",
             flat_restore_record: "flat-restore.json",
             flat_restore_result: "artifacts/flat-restore/result.json",
             flat_restore_resource_audit: "flat-restore-resource-audit.tsv",
             single_clone_pss: "single-pss.tsv",
             fleet_pss: "fleet-pss.tsv",
             pss_tsv_columns: [
               "room_id", "pid", "pid_starttime", "pss_kb", "pss_anon_kb", "pss_file_kb",
               "private_dirty_kb", "shared_clean_kb"
             ],
             fleet_record: "fleet.json",
             fleet_latency: "fleet-ready.ns",
             fleet_topology: "fleet-topology.ndjson",
             two_hop_requests: "endpoint-requests.ndjson",
             two_hop_endpoint: {
               ip: $endpoint_ip,
               port: number_or_null($endpoint_port)
             },
             guest_hygiene: "fleet-guest-evidence.tsv",
             witness_batch: "witness-batch.json",
             witness_manifest: "witness-manifest.tsv",
             terminal_roster: "final-ls.json",
             failures: "failures.ndjson",
             cleanup_log: "logs/cleanup.log"
           },
           created_room_ids: ($created_ids | split("\n") | map(select(length > 0))),
           workload_scope_note: $workload_scope_note,
           rng_scope_note: $rng_scope_note,
           failures: $failures
         }' >"$summary_tmp"
    mv "$summary_tmp" "$SUMMARY_FILE"
}

on_exit() {
    local exit_code="$?"
    local final_exit="$exit_code"
    IN_EXIT=1
    trap - ERR EXIT INT TERM HUP
    set +e

    cleanup_created_rooms
    if [[ "$CLEANUP_OK" != "true" ]]; then
        record_failure "cleanup" "cleanup_incomplete" \
            "one or more proof-owned rooms or intents could not be reaped"
        final_exit=1
    fi

    if [[ -f "$PROTECTED_BEFORE" ]]; then
        capture_protected_state "$PROTECTED_AFTER"
        if ! cmp -s "$PROTECTED_BEFORE" "$PROTECTED_AFTER"; then
            record_failure "hard" "canonical_state_changed" \
                "canonical slot or existing image fingerprint changed during the gate"
            final_exit=1
        fi
    fi

    if [[ -f "$SOURCE_MANIFEST_BEFORE" ]]; then
        source_manifest "$SOURCE_MANIFEST_FINAL"
        if ! cmp -s "$SOURCE_MANIFEST_BEFORE" "$SOURCE_MANIFEST_FINAL"; then
            record_failure "hard" "source_changed_during_gate" \
                "the source tree changed after the proof build began"
            final_exit=1
        fi
    fi

    if [[ -s "$FAILURES_FILE" ]]; then
        final_exit=1
    fi
    write_summary "$final_exit"
    log "proof artifacts preserved at $PROOF_ROOT"
    log "machine summary: $SUMMARY_FILE"
    exit "$final_exit"
}

trap 'on_error "$?" "$LINENO" "$BASH_COMMAND"' ERR
trap 'record_failure "fatal" "signal" "gate interrupted by signal"; exit 130' INT TERM HUP
trap on_exit EXIT

require_command() {
    command -v "$1" >/dev/null 2>&1 || fatal "missing_command" "required command is missing: $1"
}

monotonic_ns() {
    python3 -c 'import time; print(time.monotonic_ns())'
}

process_starttime() {
    local pid="$1"
    local stat_line

    [[ "$pid" =~ ^[0-9]+$ ]] || return 1
    stat_line="$(sudo -n cat "/proc/$pid/stat" 2>/dev/null)" || return 1
    # comm is parenthesized and may contain spaces, so strip through its final
    # closing parenthesis before counting: starttime is field 20 of the tail.
    awk '{print $20}' <<<"${stat_line##*) }"
}

read_pss_metrics() {
    local pid="$1"
    local expected_starttime="$2"
    local before_starttime
    local after_starttime
    local metrics

    [[ "$pid" =~ ^[0-9]+$ ]] || return 1
    [[ "$expected_starttime" =~ ^[0-9]+$ ]] || return 1
    before_starttime="$(process_starttime "$pid")" || return 1
    [[ "$before_starttime" == "$expected_starttime" ]] || return 1
    metrics="$(sudo -n awk '
        $1 == "Pss:" { pss = $2 }
        $1 == "Pss_Anon:" { pss_anon = $2 }
        $1 == "Pss_File:" { pss_file = $2 }
        $1 == "Private_Dirty:" { private_dirty = $2 }
        $1 == "Shared_Clean:" { shared_clean = $2 }
        END {
            if (pss == "" || pss_anon == "" || pss_file == "" ||
                private_dirty == "" || shared_clean == "" || pss <= 0) {
                exit 1
            }
            printf "%s\t%s\t%s\t%s\t%s\n",
                pss, pss_anon, pss_file, private_dirty, shared_clean
        }
    ' "/proc/$pid/smaps_rollup" 2>/dev/null)" || return 1
    after_starttime="$(process_starttime "$pid")" || return 1
    [[ "$after_starttime" == "$expected_starttime" ]] || return 1
    printf '%s\n' "$metrics"
}

assert_reservation() {
    local token
    privileged_regular_file "$STATE_DIR/slots/1" || return 1
    token="$(privileged_read_file "$STATE_DIR/slots/1")" || return 1
    [[ "$token" == "@reservation $SNAPSHOT_ID" ]]
}

ssh_clone() {
    local namespace="$1"
    local guest="$2"
    shift 2
    timeout --signal=TERM --kill-after=2s "${SSH_WALL_TIMEOUT_SECONDS}s" \
        sudo -n ip netns exec "$namespace" \
        ssh -n -i "$PROOF_HOME/.ssh/id_rooms" \
        -o BatchMode=yes \
        -o IdentitiesOnly=yes \
        -o ConnectTimeout=10 \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR \
        "rooms@$guest" "$@"
}

direct_ssh() {
    local guest="$1"
    shift
    timeout --signal=TERM --kill-after=2s "${SSH_WALL_TIMEOUT_SECONDS}s" \
        ssh -n -i "$PROOF_HOME/.ssh/id_rooms" \
        -o BatchMode=yes \
        -o IdentitiesOnly=yes \
        -o ConnectTimeout=1 \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR \
        "rooms@$guest" "$@"
}

kill_batch_exact() {
    local json_file="$1"
    local label="$2"
    local room_id
    while read -r room_id; do
        if ! run_rooms kill "$room_id" --json \
            >"$ARTIFACT_DIR/${label}-kill-$room_id.json" \
            2>>"$LOG_DIR/${label}-kill.stderr"; then
            hard_failure "exact_teardown_failed" \
                "rooms kill failed for proof-owned room $room_id"
        fi
    done < <(jq -r '.clones[].room_id' "$json_file")
}

assert_batch_owned_paths_absent() {
    local json_file="$1"
    local failed=0
    local index
    local records
    local room_id

    records="$(jq -er '.clones[] | [.room_id,.clone_net_index] | @tsv' "$json_file")" \
        || return 1
    [[ -n "$records" ]] || return 1
    while IFS=$'\t' read -r room_id index; do
        if ! privileged_paths_absent \
            "$STATE_DIR/$room_id" \
            "$STATE_DIR/jailer/firecracker/$room_id" \
            "$STATE_DIR/restore-intents/$room_id.json" \
            "$STATE_DIR/snapshot-intents/$room_id.json" \
            "$STATE_DIR/clonenets/$index"; then
            failed=1
        fi
    done <<<"$records"

    proof_transients_absent || failed=1
    ((failed == 0))
}

audit_global_clone_absence() {
    local allow_flat_tap="${1:-false}"
    local all_filter_dump
    local chain
    local failed=0
    local index
    local input_dump
    local link_dump
    local netns_dump
    local namespace_address
    local owner_state
    local veth_dump

    netns_dump="$(sudo -n ip netns list)" || return 1
    link_dump="$(sudo -n ip -o link show)" || return 1
    all_filter_dump="$(sudo -n iptables -S)" || return 1
    input_dump="$(sudo -n iptables -S INPUT)" || return 1
    veth_dump="$(sudo -n iptables -S ROOMS_VETH_FWD)" || return 1

    for index in $(seq 1 8); do
        chain="ROOMS_CEG_$index"
        namespace_address="172.17.0.$((4 * index + 2))"
        if awk '{print $1}' <<<"$netns_dump" | grep -Fxq "rooms-c$index"; then
            failed=1
        fi
        if awk -F': ' '{sub(/@.*/, "", $2); print $2}' <<<"$link_dump" \
            | grep -Exq "veth-[hg]$index"; then
            failed=1
        fi
        owner_state="$(sudo -n sh -c \
            'if [ -e "$1" ] || [ -L "$1" ]; then printf present; else printf absent; fi' \
            sh "/run/rooms/clonenet-owners/$index")" || return 1
        if [[ "$owner_state" == "present" ]]; then
            failed=1
        elif [[ "$owner_state" != "absent" ]]; then
            return 1
        fi
        if grep -Fxq -- "-N $chain" <<<"$all_filter_dump"; then
            failed=1
        fi
        if grep -Fxq -- \
            "-A ROOMS_VETH_FWD -i veth-h$index -j $chain" \
            <<<"$veth_dump"; then
            failed=1
        fi
        if grep -Fxq -- \
            "-A ROOMS_VETH_FWD -i veth-h$index ! -s $namespace_address/32 -j DROP" \
            <<<"$veth_dump"; then
            failed=1
        fi
        if grep -Fxq -- "-A INPUT -i veth-h$index -j DROP" <<<"$input_dump"; then
            failed=1
        fi
    done
    if [[ "$allow_flat_tap" != "true" ]] \
        && awk -F': ' '{sub(/@.*/, "", $2); print $2}' <<<"$link_dump" \
        | grep -Fxq tap-fc1; then
        failed=1
    fi
    ((failed == 0))
}

clone_state_claims_absent() {
    local claims_dir="$STATE_DIR/clonenets"
    local found

    if privileged_paths_absent "$claims_dir"; then
        return 0
    fi
    privileged_directory "$claims_dir" || return 1
    found="$(sudo -n find "$claims_dir" -mindepth 1 -maxdepth 1 -print -quit)" \
        || return 1
    [[ -z "$found" ]]
}

PHASE="preflight"
log "proof root: $PROOF_ROOT"

if ((EUID == 0)); then
    fatal "run_as_root" "run this gate as the ordinary rooms-host operator; it invokes sudo -n itself"
fi
if [[ "$(uname -s)" != "Linux" ]]; then
    fatal "wrong_platform" "the Phase-2 killer gate requires the Linux rooms-host"
fi

for command_name in \
    awk cargo cat chattr cksum cmp cp curl cut date debugfs find findmnt firecracker flock git grep head \
    ip iptables jailer jq lsattr mkfs.ext4 mount pgrep python3 readelf readlink rm rustc sed seq sha256sum \
    sh sleep sort ssh ssh-keygen stat sudo tar tcpdump timeout tr truncate umount wc xargs; do
    require_command "$command_name"
done
sudo -n true || fatal "sudo_unavailable" "passwordless sudo is required"
[[ -c /dev/kvm ]] || fatal "kvm_missing" "/dev/kvm is missing"
[[ -f "$CANONICAL_IMAGES/vmlinux.bin" ]] || fatal "kernel_missing" "canonical vmlinux.bin is missing"
WORST_CASE_UDS_BYTES="$(LC_ALL=C printf '%s' "$WORST_CASE_UDS_PATH" | wc -c | awk '{print $1}')"
[[ "$WORST_CASE_UDS_BYTES" =~ ^[0-9]+$ ]] \
    || fatal "sun_len_measurement_invalid" "could not measure the prospective jailed Unix-socket path"
((WORST_CASE_UDS_BYTES < SUN_LEN_BYTES)) \
    || fatal "sun_len_exceeded" \
        "prospective jailed Unix-socket path is ${WORST_CASE_UDS_BYTES} bytes; it must be shorter than SUN_LEN (${SUN_LEN_BYTES} bytes): $WORST_CASE_UDS_PATH"
sudo -n iptables -S ROOMS_FWD >/dev/null \
    || fatal "rooms_fwd_missing" "ROOMS_FWD is unavailable"
sudo -n iptables -S ROOMS_VETH_FWD >/dev/null \
    || fatal "rooms_veth_fwd_missing" "ROOMS_VETH_FWD is unavailable"
endpoint_absent \
    || fatal "proof_endpoint_busy" \
        "proof endpoint interface, address, firewall marker, or listener identity already exists"

if pgrep -x firecracker >/dev/null 2>&1 || pgrep -x jailer >/dev/null 2>&1; then
    fatal "host_busy" "a Firecracker or jailer process is already running"
fi
if sudo -n ip netns list | awk '{print $1}' | grep -Eq '^rooms-c([1-8])$'; then
    fatal "host_busy" "one of clone namespaces rooms-c1..rooms-c8 already exists"
fi
if ! audit_global_clone_absence; then
    fatal "host_resources_busy" "slot-1 or clone resources 1..8 are not globally free"
fi

capture_protected_state "$PROTECTED_BEFORE"
cp --reflink=auto --preserve=mode "$CANONICAL_IMAGES/vmlinux.bin" "$IMAGE_DIR/vmlinux.bin"
ssh-keygen -q -t ed25519 -N '' -f "$PROOF_HOME/.ssh/id_rooms"
PROOF_KEY_FINGERPRINT="$(ssh-keygen -lf "$PROOF_HOME/.ssh/id_rooms.pub" -E sha256 | awk '{print $2}')"

SOURCE_HEAD="$(git -C "$REPO_ROOT" rev-parse HEAD)"
source_manifest "$SOURCE_MANIFEST_BEFORE"
SOURCE_MANIFEST_SHA256="$(sha256sum "$SOURCE_MANIFEST_BEFORE" | awk '{print $1}')"

{
    printf 'source_head=%s\n' "$SOURCE_HEAD"
    printf 'source_manifest_sha256=%s\n' "$SOURCE_MANIFEST_SHA256"
    printf 'worst_case_uds_path=%s\n' "$WORST_CASE_UDS_PATH"
    printf 'worst_case_uds_bytes=%s\n' "$WORST_CASE_UDS_BYTES"
    printf 'rng_scope=%s\n' "$RNG_SCOPE_NOTE"
    printf 'host_arch=%s\n' "$(uname -m)"
    printf 'firecracker=%s\n' "$(firecracker --version 2>&1 | head -n 1)"
    printf 'jailer=%s\n' "$(jailer --version 2>&1 | head -n 1)"
    printf 'rustc=%s\n' "$(rustc --version)"
    printf 'cargo=%s\n' "$(cargo --version)"
} >"$PROOF_ROOT/provenance.txt"

PHASE="build"
log "building release binary and fresh snapshot-capable rootfs"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build \
    --release \
    --bin rooms \
    --manifest-path "$REPO_ROOT/Cargo.toml" \
    >"$LOG_DIR/cargo-build.stdout" \
    2>"$LOG_DIR/cargo-build.stderr"
ROOMS_BIN="$TARGET_DIR/release/rooms"
[[ -x "$ROOMS_BIN" ]] || fatal "binary_missing" "release rooms binary was not produced"

sudo -n "$SCRIPT_DIR/build-rootfs-alpine.sh" \
    --out "$IMAGE" \
    --ssh-key "$PROOF_HOME/.ssh/id_rooms.pub" \
    >"$LOG_DIR/rootfs-build.stdout" \
    2>"$LOG_DIR/rootfs-build.stderr"
lsattr -d -- "$IMAGE" | awk 'NR == 1 { exit index($1, "i") == 0 }' \
    || fatal "rootfs_not_immutable" \
        "fresh rootfs is not protected by FS_IMMUTABLE_FL"

source_manifest "$SOURCE_MANIFEST_AFTER_BUILD"
cmp "$SOURCE_MANIFEST_BEFORE" "$SOURCE_MANIFEST_AFTER_BUILD" \
    || fatal "source_changed_during_build" "source files changed while the binary/rootfs were built"

SOURCE_AGENT_SHA256="$(sha256sum "$SCRIPT_DIR/lib/rooms-resume-agent.sh" | awk '{print $1}')"
IMAGE_AGENT_SHA256="$(
    sudo -n debugfs -R 'cat /sbin/rooms-resume-agent' "$IMAGE" 2>/dev/null \
        | sha256sum \
        | awk '{print $1}'
)"
[[ "$SOURCE_AGENT_SHA256" == "$IMAGE_AGENT_SHA256" ]] \
    || fatal "resume_agent_mismatch" "fresh image does not contain the exact source resume agent"
SOURCE_APPLY_SHA256="$(sha256sum "$SCRIPT_DIR/lib/rooms-resume-apply.c" | awk '{print $1}')"
readonly IMAGE_APPLY_BINARY="$ARTIFACT_DIR/rooms-resume-apply"
sudo -n debugfs -R 'cat /sbin/rooms-resume-apply' "$IMAGE" 2>/dev/null \
    >"$IMAGE_APPLY_BINARY"
[[ -s "$IMAGE_APPLY_BINARY" ]] \
    || fatal "native_resume_apply_missing" "fresh image does not contain the native resume helper"
IMAGE_APPLY_SHA256="$(sha256sum "$IMAGE_APPLY_BINARY" | awk '{print $1}')"
if ! readelf -l "$IMAGE_APPLY_BINARY" >"$ARTIFACT_DIR/rooms-resume-apply.readelf"; then
    fatal "native_resume_apply_invalid" \
        "fresh image native resume helper is not a readable ELF binary"
fi
if grep -q 'INTERP' "$ARTIFACT_DIR/rooms-resume-apply.readelf"; then
    fatal "native_resume_apply_dynamic" \
        "fresh image native resume helper has a dynamic interpreter"
fi
printf 'source_c_sha256\t%s\nimage_binary_sha256\t%s\n' \
    "$SOURCE_APPLY_SHA256" "$IMAGE_APPLY_SHA256" \
    >"$PROOF_ROOT/native-resume-artifacts.tsv"

for key_type in rsa ecdsa ed25519; do
    if sudo -n debugfs \
        -R "stat /etc/ssh/ssh_host_${key_type}_key" \
        "$IMAGE" 2>/dev/null | grep -q '^Inode:'; then
        fatal "baked_host_key" "fresh rootfs contains a private SSH host key ($key_type)"
    fi
done

ROOTFS_SHA256="$(sha256sum "$IMAGE" | awk '{print $1}')"
sha256sum "$ROOMS_BIN" "$IMAGE" "$IMAGE_DIR/vmlinux.bin" "$IMAGE_APPLY_BINARY" \
    >"$PROOF_ROOT/build-artifacts.sha256"
IMAGE_VERIFICATION_PASS="true"

PHASE="neutral-base"
log "creating a credential-free warmed neutral base"
run_rooms base-create \
    --image "$IMAGE" \
    --repo "$REPO_ROOT" \
    --warm "test \"\$(git -C /workspace/repo rev-parse HEAD)\" = \"$SOURCE_HEAD\" && claude --version >/dev/null" \
    --max-pool 8 \
    --json \
    >"$PROOF_ROOT/base.json" \
    2>"$LOG_DIR/base.stderr"

BASE_ID="$(jq -er .room_id "$PROOF_ROOT/base.json")"
readonly BASE_ID
add_created_id "$BASE_ID" || fatal "invalid_base_id" "base-create returned an invalid room id"
jq -e '.slot == 1 and .provenance == "neutral"' "$PROOF_ROOT/base.json" >/dev/null \
    || fatal "base_not_neutral" "base-create did not produce neutral slot-1 base metadata"

PHASE="snapshot"
log "freezing the neutral base"
run_rooms snapshot "$BASE_ID" \
    --out "$SNAPSHOT_DIR" \
    --json \
    >"$PROOF_ROOT/snapshot-result.json" \
    2>"$LOG_DIR/snapshot.stderr"

SNAPSHOT_ID="$(jq -er .snapshot_id "$PROOF_ROOT/snapshot-result.json")"
valid_room_id "$SNAPSHOT_ID" || fatal "invalid_snapshot_id" "snapshot returned an invalid id"
jq -e '.slot == 1' "$PROOF_ROOT/snapshot-result.json" >/dev/null \
    || fatal "snapshot_wrong_slot" "snapshot did not retain frozen slot 1"
for artifact in snapshot.json snapshot.mem snapshot.vmstate; do
    privileged_nonempty_file "$SNAPSHOT_DIR/$artifact" \
        || fatal "snapshot_artifact_missing" "$artifact is missing or empty"
    sudo -n lsattr -d -- "$SNAPSHOT_DIR/$artifact" \
        | awk 'NR == 1 { exit index($1, "i") == 0 }' \
        || fatal "snapshot_artifact_not_immutable" \
            "$artifact is not protected by FS_IMMUTABLE_FL"
done
privileged_directory "$SNAPSHOT_DIR" \
    || fatal "snapshot_directory_invalid" \
        "published snapshot path is not an exact directory"
sudo -n lsattr -d -- "$SNAPSHOT_DIR" \
    | awk 'NR == 1 { exit index($1, "i") == 0 }' \
    || fatal "snapshot_directory_not_immutable" \
        "published snapshot directory is not protected by FS_IMMUTABLE_FL"
sudo -n jq -e \
    --arg rootfs_hash "sha256:$ROOTFS_SHA256" \
    --arg snapshot_id "$SNAPSHOT_ID" \
    --arg base_id "$BASE_ID" \
    --arg source_head "$SOURCE_HEAD" \
    '.snapshot_id == $snapshot_id and
     .rootfs_hash == $rootfs_hash and
     .base_room_id == $base_id and
     (.base_repo_sha == null or .base_repo_sha == $source_head) and
     .slot_index == 1 and
     (.guest_ip | type == "string" and length > 0) and
     .provenance == "neutral"' \
    "$SNAPSHOT_DIR/snapshot.json" >/dev/null \
    || fatal "snapshot_metadata_mismatch" "snapshot metadata does not pin the fresh image and neutral provenance"
readonly SNAPSHOT_ATTESTATION="$STATE_DIR/snapshot-attestations/$SNAPSHOT_ID.json"
privileged_nonempty_file "$SNAPSHOT_ATTESTATION" \
    || fatal "snapshot_attestation_missing" \
        "snapshot publication did not produce its separate local compatibility receipt"
sudo -n lsattr -d -- "$SNAPSHOT_ATTESTATION" \
    | awk 'NR == 1 { exit index($1, "i") == 0 }' \
    || fatal "snapshot_attestation_not_immutable" \
        "snapshot compatibility receipt is not protected by FS_IMMUTABLE_FL"
read -r snapshot_dir_device snapshot_dir_inode snapshot_dir_len \
    <<<"$(sudo -n stat -Lc '%d %i %s' "$SNAPSHOT_DIR")"
read -r snapshot_meta_device snapshot_meta_inode snapshot_meta_len \
    <<<"$(sudo -n stat -Lc '%d %i %s' "$SNAPSHOT_DIR/snapshot.json")"
read -r rootfs_device rootfs_inode rootfs_len \
    <<<"$(sudo -n stat -Lc '%d %i %s' "$IMAGE")"
sudo -n jq -e \
    --arg snapshot_dir "$(readlink -f "$SNAPSHOT_DIR")" \
    --argjson snapshot_dir_device "$snapshot_dir_device" \
    --argjson snapshot_dir_inode "$snapshot_dir_inode" \
    --argjson snapshot_dir_len "$snapshot_dir_len" \
    --argjson snapshot_meta_device "$snapshot_meta_device" \
    --argjson snapshot_meta_inode "$snapshot_meta_inode" \
    --argjson snapshot_meta_len "$snapshot_meta_len" \
    --argjson rootfs_device "$rootfs_device" \
    --argjson rootfs_inode "$rootfs_inode" \
    --argjson rootfs_len "$rootfs_len" \
    --slurpfile portable "$SNAPSHOT_DIR/snapshot.json" '
    .schema_version == 1 and
    .snapshot_dir == $snapshot_dir and
    .meta == $portable[0] and
    .snapshot_dir_source.device == $snapshot_dir_device and
    .snapshot_dir_source.inode == $snapshot_dir_inode and
    .snapshot_dir_source.len == $snapshot_dir_len and
    .snapshot_meta_source.device == $snapshot_meta_device and
    .snapshot_meta_source.inode == $snapshot_meta_inode and
    .snapshot_meta_source.len == $snapshot_meta_len and
    .rootfs_source.device == $rootfs_device and
    .rootfs_source.inode == $rootfs_inode and
    .rootfs_source.len == $rootfs_len and
    ([.snapshot_dir_source, .snapshot_meta_source, .rootfs_source] |
      all(.[];
        (.device | type == "number") and
        (.inode | type == "number") and
        (.len | type == "number" and . > 0) and
        (.mode | type == "number") and
        (.mtime | type == "number") and
        (.mtime_nsec | type == "number") and
        (.ctime | type == "number") and
        (.ctime_nsec | type == "number")))' \
    "$SNAPSHOT_ATTESTATION" >/dev/null \
    || fatal "snapshot_attestation_invalid" \
        "snapshot compatibility receipt does not bind the exact portable metadata and local sources"
SNAPSHOT_ATTESTATION_SHA256="$(sudo -n sha256sum "$SNAPSHOT_ATTESTATION" | awk '{print $1}')"
SNAPSHOT_GUEST="$(sudo -n jq -er .guest_ip "$SNAPSHOT_DIR/snapshot.json")"
BASE_REPO_SHA="$SOURCE_HEAD"
assert_reservation \
    || fatal "reservation_missing" "proof state does not hold the exact snapshot reservation"
SNAPSHOT_MEM_BYTES="$(sudo -n stat -Lc %s "$SNAPSHOT_DIR/snapshot.mem")"
SNAPSHOT_MEM_INODE="$(sudo -n stat -Lc '%d:%i' "$SNAPSHOT_DIR/snapshot.mem")"
RESERVATION_SHA256="$(sudo -n sha256sum "$STATE_DIR/slots/1" | awk '{print $1}')"
readonly RESERVATION_SHA256
sudo -n sha256sum \
    "$SNAPSHOT_DIR/snapshot.json" \
    "$SNAPSHOT_DIR/snapshot.mem" \
    "$SNAPSHOT_DIR/snapshot.vmstate" \
    "$SNAPSHOT_ATTESTATION" \
    >"$PROOF_ROOT/snapshot-artifacts.sha256"
SNAPSHOT_PASS="true"

PHASE="flat-restore-regression"
log "proving the unchanged flat rooms restore command path"
printf 'check\tvalue\n' >"$FLAT_RESTORE_AUDIT"
flat_restore_status=0
(
    set +e
    trap - ERR
    run_rooms restore "$SNAPSHOT_DIR" \
        --image "$IMAGE" \
        --command "sleep 8 && test \"\$(git -C /workspace/repo rev-parse HEAD)\" = \"$SOURCE_HEAD\" && printf '%s\\n' flat-restore-ok > /workspace/out/flat-restore.ok" \
        --out "$FLAT_RESTORE_OUT" \
        --json
) >"$FLAT_RESTORE_JSON" 2>"$LOG_DIR/flat-restore.stderr" &
flat_restore_pid=$!

flat_restore_candidate=""
for _attempt in $(seq 1 600); do
    if flat_restore_candidate="$(jq -er '.room_id' "$FLAT_RESTORE_JSON" 2>/dev/null)" \
        && valid_room_id "$flat_restore_candidate"; then
        FLAT_RESTORE_ID="$flat_restore_candidate"
        break
    fi
    if ! kill -0 "$flat_restore_pid" 2>/dev/null; then
        break
    fi
    sleep 0.1
done

flat_meta_ok="false"
flat_root_tap_ok="false"
flat_claims_ok="false"
flat_host_resources_ok="false"
if valid_room_id "$FLAT_RESTORE_ID"; then
    add_created_id "$FLAT_RESTORE_ID" \
        || fatal "invalid_flat_restore_id" "flat restore returned an invalid room id"
    if privileged_nonempty_file "$STATE_DIR/$FLAT_RESTORE_ID/room.json" \
        && sudo -n jq -e \
            --arg room_id "$FLAT_RESTORE_ID" \
            --arg snapshot_id "$SNAPSHOT_ID" \
            '.id == $room_id and
             .slot.index == 1 and
             .snapshot_lineage == $snapshot_id and
             .clone_net_index == null' \
            "$STATE_DIR/$FLAT_RESTORE_ID/room.json" >/dev/null; then
        flat_meta_ok="true"
    fi
    if sudo -n ip link show tap-fc1 >/dev/null 2>&1; then
        flat_root_tap_ok="true"
    fi
    if clone_state_claims_absent; then
        flat_claims_ok="true"
    fi
    if audit_global_clone_absence true; then
        flat_host_resources_ok="true"
    fi
fi
printf 'live_room_id\t%s\n' "$FLAT_RESTORE_ID" >>"$FLAT_RESTORE_AUDIT"
printf 'live_room_metadata_flat\t%s\n' "$flat_meta_ok" >>"$FLAT_RESTORE_AUDIT"
printf 'live_root_tap_present\t%s\n' "$flat_root_tap_ok" >>"$FLAT_RESTORE_AUDIT"
printf 'live_clone_state_claims_absent\t%s\n' "$flat_claims_ok" >>"$FLAT_RESTORE_AUDIT"
printf 'live_clone_host_resources_absent\t%s\n' \
    "$flat_host_resources_ok" >>"$FLAT_RESTORE_AUDIT"
if [[ "$flat_meta_ok" == "true" \
    && "$flat_root_tap_ok" == "true" \
    && "$flat_claims_ok" == "true" \
    && "$flat_host_resources_ok" == "true" ]]; then
    FLAT_RESTORE_LIVE_FLAT_PASS="true"
fi

wait "$flat_restore_pid" || flat_restore_status=$?
if ! valid_room_id "$FLAT_RESTORE_ID"; then
    flat_restore_candidate="$(jq -er '.room_id' "$FLAT_RESTORE_JSON" 2>/dev/null || true)"
    if valid_room_id "$flat_restore_candidate"; then
        FLAT_RESTORE_ID="$flat_restore_candidate"
        add_created_id "$FLAT_RESTORE_ID" \
            || fatal "invalid_flat_restore_id" "flat restore returned an invalid room id"
    fi
fi
if ((flat_restore_status != 0)); then
    fatal "flat_restore_command_failed" \
        "flat rooms restore command mode exited $flat_restore_status"
fi
if ! valid_room_id "$FLAT_RESTORE_ID"; then
    fatal "flat_restore_record_missing" \
        "flat rooms restore did not emit a valid live room record"
fi
if [[ "$FLAT_RESTORE_LIVE_FLAT_PASS" != "true" ]]; then
    fatal "flat_restore_used_clone_network" \
        "flat restore did not remain on the root tap with clone state and host resources absent"
fi
jq -e \
    --arg room_id "$FLAT_RESTORE_ID" \
    --arg snapshot_id "$SNAPSHOT_ID" \
    --arg guest "$SNAPSHOT_GUEST" \
    '.room_id == $room_id and
     .snapshot_id == $snapshot_id and
     .slot == 1 and
     .guest_ip == $guest' \
    "$FLAT_RESTORE_JSON" >/dev/null \
    || fatal "flat_restore_record_invalid" \
        "flat restore returned a record that does not match the exact snapshot reservation"
privileged_nonempty_file "$FLAT_RESTORE_OUT/result.json" \
    || fatal "flat_restore_result_missing" \
        "flat restore did not collect result.json"
sudo -n jq -e \
    '.schema_version == 1 and .status == "succeeded" and .exit_code == 0' \
    "$FLAT_RESTORE_OUT/result.json" >/dev/null \
    || fatal "flat_restore_result_invalid" \
        "flat restore result.json did not record a successful command"
privileged_nonempty_file "$FLAT_RESTORE_OUT/flat-restore.ok" \
    && [[ "$(privileged_read_file "$FLAT_RESTORE_OUT/flat-restore.ok")" == "flat-restore-ok" ]] \
    || fatal "flat_restore_marker_invalid" \
        "flat restore did not collect the exact workload marker"
FLAT_RESTORE_COMMAND_PASS="true"

assert_terminal_proof_slot \
    || fatal "flat_restore_reservation_drift" \
        "flat restore did not return the exact snapshot reservation token and digest"
FLAT_RESTORE_RESERVATION_PASS="true"
if ! privileged_paths_absent \
    "$STATE_DIR/$FLAT_RESTORE_ID" \
    "$STATE_DIR/jailer/firecracker/$FLAT_RESTORE_ID" \
    "$STATE_DIR/restore-intents/$FLAT_RESTORE_ID.json" \
    "$STATE_DIR/snapshot-intents/$FLAT_RESTORE_ID.json"; then
    fatal "flat_restore_owned_path_leak" \
        "flat restore left a room, jail, or transaction path"
fi
proof_transients_absent \
    || fatal "flat_restore_transient_leak" \
        "flat restore left an intent, attestation temp, or clone-network claim"
clone_state_claims_absent \
    || fatal "flat_restore_clone_claim_leak" \
        "flat restore allocated or leaked a clone-network state claim"
audit_global_clone_absence \
    || fatal "flat_restore_host_resource_leak" \
        "flat restore left a tap, clone namespace, veth, owner, or firewall resource"
if pgrep -x firecracker >/dev/null 2>&1 || pgrep -x jailer >/dev/null 2>&1; then
    fatal "flat_restore_process_leak" "flat restore left a Firecracker or jailer process"
fi
if findmnt -rn -o TARGET | grep -Fq "$STATE_DIR/jailer/firecracker/$FLAT_RESTORE_ID"; then
    fatal "flat_restore_mount_leak" "flat restore left a jail bind mount"
fi
printf 'terminal_reservation_returned\ttrue\n' >>"$FLAT_RESTORE_AUDIT"
printf 'terminal_owned_paths_absent\ttrue\n' >>"$FLAT_RESTORE_AUDIT"
printf 'terminal_clone_resources_absent\ttrue\n' >>"$FLAT_RESTORE_AUDIT"
printf 'terminal_vmm_and_mounts_absent\ttrue\n' >>"$FLAT_RESTORE_AUDIT"
FLAT_RESTORE_CLEANUP_PASS="true"
FLAT_RESTORE_PASS="true"
log "PASS: flat restore command path used root tap-fc1, returned the exact reservation, and left no clone resources"

PHASE="single-clone-baseline"
log "measuring one-clone PSS baseline"
run_rooms clone "$SNAPSHOT_DIR" \
    --image "$IMAGE" \
    -n 1 \
    --max-pool 8 \
    --egress none \
    --json \
    >"$PROOF_ROOT/single.json" \
    2>"$LOG_DIR/single.stderr"
track_json_ids "$PROOF_ROOT/single.json"
jq -e \
    --arg snapshot_id "$SNAPSHOT_ID" \
    --arg guest "$SNAPSHOT_GUEST" \
    '(.clones | length) == 1 and
     .clones[0].status == "kept" and
     .clones[0].snapshot_id == $snapshot_id and
     .clones[0].slot == 1 and
     .clones[0].guest_ip == $guest and
     .clones[0].clone_net_index == 1' \
    "$PROOF_ROOT/single.json" >/dev/null \
    || fatal "single_clone_record_invalid" "one-clone baseline returned an invalid record"

SINGLE_ID="$(jq -er '.clones[0].room_id' "$PROOF_ROOT/single.json")"
readonly SINGLE_ID
SINGLE_NAMESPACE="$(jq -er '.clones[0].namespace' "$PROOF_ROOT/single.json")"
readonly SINGLE_NAMESPACE
SINGLE_GUEST="$(jq -er '.clones[0].guest_ip' "$PROOF_ROOT/single.json")"
readonly SINGLE_GUEST
ssh_clone "$SINGLE_NAMESPACE" "$SINGLE_GUEST" true \
    >"$LOG_DIR/single-ready.stdout" \
    2>"$LOG_DIR/single-ready.stderr" \
    || fatal "single_clone_not_ready" "one-clone baseline did not accept SSH"
IFS=$'\t' read -r SINGLE_PID SINGLE_PID_STARTTIME \
    <<<"$(sudo -n jq -er '[.pid,.pid_starttime] | @tsv' "$STATE_DIR/$SINGLE_ID/room.json")"
readonly SINGLE_PID
readonly SINGLE_PID_STARTTIME
single_pss_metrics="$(read_pss_metrics "$SINGLE_PID" "$SINGLE_PID_STARTTIME")" \
    || fatal "single_pss_invalid" "one-clone PSS could not be measured"
IFS=$'\t' read -r \
    SINGLE_PSS_KB \
    SINGLE_PSS_ANON_KB \
    SINGLE_PSS_FILE_KB \
    SINGLE_PRIVATE_DIRTY_KB \
    SINGLE_SHARED_CLEAN_KB \
    <<<"$single_pss_metrics"
printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$SINGLE_ID" \
    "$SINGLE_PID" \
    "$SINGLE_PID_STARTTIME" \
    "$SINGLE_PSS_KB" \
    "$SINGLE_PSS_ANON_KB" \
    "$SINGLE_PSS_FILE_KB" \
    "$SINGLE_PRIVATE_DIRTY_KB" \
    "$SINGLE_SHARED_CLEAN_KB" \
    >"$PROOF_ROOT/single-pss.tsv"

kill_batch_exact "$PROOF_ROOT/single.json" "single"
assert_reservation \
    || fatal "single_reservation_drift" "one-clone teardown did not return the exact reservation"
assert_batch_owned_paths_absent "$PROOF_ROOT/single.json" \
    || fatal "single_owned_path_leak" \
        "one-clone teardown left a proof-owned room, jail, intent, or claim path"
audit_global_clone_absence \
    || fatal "single_teardown_leak" "one-clone baseline leaked host resources"
if ((HARD_FAILURES > 0)); then
    fatal "single_teardown_failed" "one-clone teardown reported an exact-owner cleanup failure"
fi

PHASE="two-hop-endpoint-setup"
log "starting the proof-owned root-namespace round-trip endpoint"
start_proof_endpoint \
    || fatal "proof_endpoint_setup_failed" \
        "could not create the exact dummy address, listener, and INPUT allowance"

PHASE="eight-clone-live"
log "launching the timed kept eight-clone observe-mode fleet"
readonly FLEET_JSON="$PROOF_ROOT/fleet.json"
RESUME_SECRET_VALUE="phase2-$(printf '%s' "$PROOF_ROOT:$SOURCE_HEAD:$(monotonic_ns)" \
    | sha256sum | cut -c1-32)"
secret_scan_status=0
sudo -n grep -aFq -- "$RESUME_SECRET_VALUE" \
    "$SNAPSHOT_DIR/snapshot.json" \
    "$SNAPSHOT_DIR/snapshot.mem" \
    "$SNAPSHOT_DIR/snapshot.vmstate" \
    || secret_scan_status=$?
if ((secret_scan_status == 0)); then
    fatal "post_resume_secret_in_snapshot" \
        "fresh post-resume secret unexpectedly appears in a neutral snapshot artifact"
elif ((secret_scan_status != 1)); then
    fatal "post_resume_secret_scan_failed" \
        "could not scan every neutral snapshot artifact for the post-resume secret"
fi
export ROOMS_PHASE2_PROBE="$RESUME_SECRET_VALUE"
FLEET_START_NS="$(monotonic_ns)"
readonly FLEET_START_NS
run_rooms clone "$SNAPSHOT_DIR" \
    --image "$IMAGE" \
    -n 8 \
    --max-pool 8 \
    --secret ROOMS_PHASE2_PROBE \
    --json \
    >"$FLEET_JSON" \
    2>"$LOG_DIR/fleet.stderr"
unset ROOMS_PHASE2_PROBE
track_json_ids "$FLEET_JSON"

jq -e \
    --arg snapshot_id "$SNAPSHOT_ID" \
    --arg guest "$SNAPSHOT_GUEST" '
    (.clones | length) == 8 and
    all(.clones[];
        .status == "kept" and
        .snapshot_id == $snapshot_id and
        .slot == 1 and
        .guest_ip == $guest) and
    ([.clones[].room_id] | unique | length) == 8 and
    ([.clones[].namespace] | unique | length) == 8 and
    ([.clones[].host_veth] | unique | length) == 8 and
    ([.clones[].clone_net_index] | sort) == [range(1; 9)]
' "$FLEET_JSON" >/dev/null \
    || fatal "fleet_record_invalid" "kept fleet did not return eight exact unique clone identities"

declare -a readiness_pids=()
declare -a readiness_ids=()
while IFS=$'\t' read -r room_id namespace guest; do
    (
        # A failed readiness probe is collected by `wait` below. The ERR trap
        # is inherited into asynchronous subshells under `set -E`; disable it
        # here so an expected non-zero SSH status is recorded exactly once.
        trap - ERR
        ssh_clone "$namespace" "$guest" true
    ) \
        >"$LOG_DIR/ready-$room_id.stdout" \
        2>"$LOG_DIR/ready-$room_id.stderr" &
    readiness_pids+=("$!")
    readiness_ids+=("$room_id")
done < <(jq -r '.clones[] | [.room_id,.namespace,.guest_ip] | @tsv' "$FLEET_JSON")

READINESS_PASS="true"
for position in "${!readiness_pids[@]}"; do
    readiness_status=0
    wait "${readiness_pids[$position]}" || readiness_status=$?
    if ((readiness_status != 0)); then
        READINESS_PASS="false"
        hard_failure "clone_not_workload_ready" \
            "clone ${readiness_ids[$position]} did not accept its namespaced SSH probe (exit $readiness_status)"
    fi
done

# A kept clone has completed resume hygiene but has not traversed the workload
# channel. The literal gate ends only after all eight authenticated namespaced
# SSH probes complete, so it measures the real CLI plus its readiness gate.
FLEET_READY_NS="$(monotonic_ns)"
readonly FLEET_READY_NS
FLEET_ELAPSED_NS="$((FLEET_READY_NS - FLEET_START_NS))"
((FLEET_ELAPSED_NS >= 0)) \
    || fatal "monotonic_clock_invalid" "monotonic clone interval was negative"
printf '%s\n' "$FLEET_ELAPSED_NS" >"$PROOF_ROOT/fleet-ready.ns"
if [[ "$READINESS_PASS" == "true" ]] && ((FLEET_ELAPSED_NS < 1000000000)); then
    LITERAL_LATENCY_PASS="true"
else
    LITERAL_LATENCY_PASS="false"
    performance_failure "fleet_not_under_one_second" \
        "the eight-clone CLI plus authenticated readiness gate completed in ${FLEET_ELAPSED_NS}ns; literal requirement is <1s (<1000000000ns)"
fi

PHASE="fleet-memory"
log "capturing workload-ready fleet PSS before any two-hop probe"
readonly FLEET_PSS_TSV="$PROOF_ROOT/fleet-pss.tsv"
: >"$FLEET_PSS_TSV"
PSS_PASS="true"
while read -r room_id; do
    room_json="$STATE_DIR/$room_id/room.json"
    privileged_regular_file "$room_json" \
        || fatal "fleet_pss_room_metadata_missing" \
            "room metadata disappeared before PSS capture for $room_id"
    IFS=$'\t' read -r pid pid_starttime \
        <<<"$(sudo -n jq -er '[.pid,.pid_starttime] | @tsv' "$room_json")"
    fleet_clone_pss_metrics="$(read_pss_metrics "$pid" "$pid_starttime")" \
        || fatal "clone_pss_invalid" "clone PSS could not be measured for $room_id"
    IFS=$'\t' read -r \
        pss_kb \
        pss_anon_kb \
        pss_file_kb \
        private_dirty_kb \
        shared_clean_kb \
        <<<"$fleet_clone_pss_metrics"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$room_id" \
        "$pid" \
        "$pid_starttime" \
        "$pss_kb" \
        "$pss_anon_kb" \
        "$pss_file_kb" \
        "$private_dirty_kb" \
        "$shared_clean_kb" \
        >>"$FLEET_PSS_TSV"
done < <(jq -r '.clones[].room_id' "$FLEET_JSON")

if (( $(wc -l <"$FLEET_PSS_TSV") != 8 )); then
    fatal "fleet_pss_incomplete" "did not capture eight workload-ready Firecracker PSS records"
fi
fleet_pss_totals="$(awk -F '\t' '
    {
        pss += $4
        pss_anon += $5
        pss_file += $6
        private_dirty += $7
        shared_clean += $8
    }
    END {
        printf "%s\t%s\t%s\t%s\t%s\n",
            pss, pss_anon, pss_file, private_dirty, shared_clean
    }
' "$FLEET_PSS_TSV")"
IFS=$'\t' read -r \
    FLEET_PSS_KB \
    FLEET_PSS_ANON_KB \
    FLEET_PSS_FILE_KB \
    FLEET_PRIVATE_DIRTY_KB \
    FLEET_SHARED_CLEAN_KB \
    <<<"$fleet_pss_totals"
[[ "$FLEET_PSS_KB" =~ ^[0-9]+$ ]] && ((FLEET_PSS_KB > 0)) \
    || fatal "fleet_pss_invalid" "eight-clone aggregate PSS could not be measured"
PSS_SHARING_RATIO="$(awk -v fleet="$FLEET_PSS_KB" -v single="$SINGLE_PSS_KB" \
    'BEGIN { printf "%.6f", fleet / (8 * single) }')"
if ((FLEET_PSS_KB >= SINGLE_PSS_KB * 2)); then
    PSS_PASS="false"
    performance_failure "pss_density_missed" \
        "fleet PSS ${FLEET_PSS_KB}KiB is not below 2x one-clone PSS ${SINGLE_PSS_KB}KiB"
fi

PHASE="fleet-two-hop-return-path"
log "proving request and response across guest tap, namespace NAT, veth, and root INPUT"
readonly TWO_HOP_GUEST_EVIDENCE="$PROOF_ROOT/two-hop-guest-evidence.tsv"
: >"$TWO_HOP_GUEST_EVIDENCE"
TWO_HOP_PASS="true"
while IFS=$'\t' read -r room_id index namespace guest; do
    expected_response="$ENDPOINT_TOKEN:$room_id"
    expected_peer="172.17.0.$((4 * index + 2))"
    if ! endpoint_response="$(ssh_clone "$namespace" "$guest" \
        "command -v curl >/dev/null && curl --fail --silent --show-error --max-time 3 --noproxy '*' http://$ENDPOINT_IP:$ENDPOINT_PORT/$room_id" \
        2>"$LOG_DIR/two-hop-$room_id.stderr")"; then
        TWO_HOP_PASS="false"
        hard_failure "two_hop_request_failed" \
            "clone $room_id could not complete the hermetic root-endpoint round trip"
        continue
    fi
    printf '%s\t%s\t%s\t%s\n' \
        "$room_id" "$index" "$expected_peer" "$endpoint_response" \
        >>"$TWO_HOP_GUEST_EVIDENCE"
    if [[ "$endpoint_response" != "$expected_response" ]]; then
        TWO_HOP_PASS="false"
        hard_failure "two_hop_response_mismatch" \
            "clone $room_id did not receive its exact endpoint response"
    fi
done < <(
    jq -r '.clones[] | [.room_id,.clone_net_index,.namespace,.guest_ip] | @tsv' "$FLEET_JSON"
)

if (( $(wc -l <"$TWO_HOP_GUEST_EVIDENCE") != 8 )); then
    TWO_HOP_PASS="false"
    hard_failure "two_hop_guest_count_wrong" \
        "did not collect eight successful guest round-trip responses"
fi
if ! jq -se 'length == 8 and ([.[].path] | unique | length) == 8' \
    "$ENDPOINT_REQUESTS" >/dev/null; then
    TWO_HOP_PASS="false"
    hard_failure "two_hop_request_log_invalid" \
        "proof endpoint did not record eight distinct request paths"
fi
while IFS=$'\t' read -r room_id index; do
    expected_peer="172.17.0.$((4 * index + 2))"
    if ! jq -se \
        --arg path "/$room_id" \
        --arg peer "$expected_peer" \
        '[.[] | select(.path == $path and .peer == $peer)] | length == 1' \
        "$ENDPOINT_REQUESTS" >/dev/null; then
        TWO_HOP_PASS="false"
        hard_failure "two_hop_nat_identity_wrong" \
            "endpoint did not see exactly one $expected_peer NAT request for $room_id"
    fi
done < <(jq -r '.clones[] | [.room_id,.clone_net_index] | @tsv' "$FLEET_JSON")
if ! cleanup_proof_endpoint; then
    TWO_HOP_PASS="false"
    hard_failure "proof_endpoint_cleanup_failed" \
        "proof-owned listener, INPUT rule, dummy address, or interface did not clean exactly"
fi
if ! endpoint_absent; then
    TWO_HOP_PASS="false"
    hard_failure "proof_endpoint_leaked" \
        "proof-owned endpoint resource remains after the return-path proof"
fi

PHASE="fleet-topology"
log "capturing namespace, custody, and shared-inode evidence"
readonly TOPOLOGY_NDJSON="$PROOF_ROOT/fleet-topology.ndjson"
: >"$TOPOLOGY_NDJSON"
TOPOLOGY_PASS="true"
while IFS=$'\t' read -r room_id namespace index host_veth slot; do
    room_json="$STATE_DIR/$room_id/room.json"
    if ! privileged_regular_file "$room_json"; then
        TOPOLOGY_PASS="false"
        hard_failure "room_metadata_missing" "room metadata disappeared for $room_id"
        continue
    fi
    IFS=$'\t' read -r pid pid_starttime \
        <<<"$(sudo -n jq -er '[.pid,.pid_starttime] | @tsv' "$room_json")"
    process_ns_inode="$(sudo -n stat -Lc %i "/proc/$pid/ns/net")"
    named_ns_inode="$(sudo -n stat -Lc %i "/run/netns/$namespace")"
    pss_record="$(awk -F '\t' -v room_id="$room_id" '
        $1 == room_id { print; found = 1; exit }
        END { if (!found) exit 1 }
    ' "$FLEET_PSS_TSV")" \
        || fatal "fleet_pss_record_missing" "no workload-ready PSS record exists for $room_id"
    IFS=$'\t' read -r \
        pss_room_id \
        pss_pid \
        pss_pid_starttime \
        pss_kb \
        pss_anon_kb \
        pss_file_kb \
        private_dirty_kb \
        shared_clean_kb \
        <<<"$pss_record"
    [[ "$pss_room_id" == "$room_id" \
        && "$pss_pid" == "$pid" \
        && "$pss_pid_starttime" == "$pid_starttime" \
        && "$(process_starttime "$pid")" == "$pid_starttime" ]] \
        || fatal "fleet_pss_identity_changed" \
            "room process identity changed after workload-ready PSS capture for $room_id"
    mem_inode="$(sudo -n stat -Lc '%d:%i' "$STATE_DIR/jailer/firecracker/$room_id/root/snapshot.mem")"
    netns_octet=$((4 * index + 2))
    route_base=$((4 * index))
    namespace_veth="veth-g$index"
    chain="ROOMS_CEG_$index"
    topology_ok="true"

    if [[ "$process_ns_inode" != "$named_ns_inode" ]]; then
        topology_ok="false"
    fi
    if [[ "$mem_inode" != "$SNAPSHOT_MEM_INODE" ]]; then
        topology_ok="false"
    fi
    if ! sudo -n ip -n "$namespace" link show "tap-fc$slot" >/dev/null 2>&1; then
        topology_ok="false"
    fi
    if ! sudo -n ip link show "$host_veth" >/dev/null 2>&1; then
        topology_ok="false"
    fi
    if ! sudo -n ip -n "$namespace" link show "$namespace_veth" >/dev/null 2>&1; then
        topology_ok="false"
    fi
    if ! sudo -n ip -n "$namespace" -4 -o addr show dev "$namespace_veth" \
        | grep -Fq "inet 172.17.0.$netns_octet/30"; then
        topology_ok="false"
    fi
    if ! sudo -n ip -n "$namespace" route show default \
        | grep -Fq "via 172.17.0.$((route_base + 1)) dev $namespace_veth"; then
        topology_ok="false"
    fi
    if ! sudo -n ip route show "172.17.0.$route_base/30" dev "$host_veth" \
        | grep -Fq "src 172.17.0.$((route_base + 1))"; then
        topology_ok="false"
    fi
    if ! sudo -n ip netns exec "$namespace" iptables -t nat -C POSTROUTING \
        -s 172.16.0.0/24 -o "$namespace_veth" -j MASQUERADE >/dev/null 2>&1; then
        topology_ok="false"
    fi
    if ! sudo -n iptables -C ROOMS_VETH_FWD \
        -i "$host_veth" '!' -s "172.17.0.$netns_octet/32" -j DROP >/dev/null 2>&1; then
        topology_ok="false"
    fi
    if sudo -n iptables -C ROOMS_VETH_FWD \
        -i "$host_veth" -j "$chain" >/dev/null 2>&1; then
        topology_ok="false"
    fi
    if sudo -n iptables -C INPUT -i "$host_veth" -j DROP >/dev/null 2>&1; then
        topology_ok="false"
    fi
    if sudo -n iptables -S "$chain" >/dev/null 2>&1; then
        topology_ok="false"
    fi
    if [[ "$topology_ok" != "true" ]]; then
        TOPOLOGY_PASS="false"
        hard_failure "clone_topology_invalid" \
            "observe-mode namespace/veth/NAT custody, absent enforcement, or source snapshot inode is invalid for $room_id"
    fi

    jq -cn \
        --arg room_id "$room_id" \
        --argjson clone_net_index "$index" \
        --arg namespace "$namespace" \
        --arg host_veth "$host_veth" \
        --argjson pid "$pid" \
        --argjson pid_starttime "$pid_starttime" \
        --arg process_ns_inode "$process_ns_inode" \
        --arg named_ns_inode "$named_ns_inode" \
        --arg mem_inode "$mem_inode" \
        --argjson pss_kb "$pss_kb" \
        --argjson pss_anon_kb "$pss_anon_kb" \
        --argjson pss_file_kb "$pss_file_kb" \
        --argjson private_dirty_kb "$private_dirty_kb" \
        --argjson shared_clean_kb "$shared_clean_kb" \
        --argjson topology_ok "$topology_ok" \
        '{room_id:$room_id, clone_net_index:$clone_net_index, namespace:$namespace,
          host_veth:$host_veth, pid:$pid, pid_starttime:$pid_starttime,
          process_ns_inode:$process_ns_inode,
          named_ns_inode:$named_ns_inode, snapshot_mem_inode:$mem_inode,
          pss_kb:$pss_kb, pss_anon_kb:$pss_anon_kb, pss_file_kb:$pss_file_kb,
          private_dirty_kb:$private_dirty_kb, shared_clean_kb:$shared_clean_kb,
          topology_ok:$topology_ok}' \
        >>"$TOPOLOGY_NDJSON"
done < <(
    jq -r '.clones[] | [.room_id,.namespace,.clone_net_index,.host_veth,.slot] | @tsv' \
        "$FLEET_JSON"
)

if sudo -n ip link show tap-fc1 >/dev/null 2>&1; then
    TOPOLOGY_PASS="false"
    hard_failure "tap_escaped_namespace" "tap-fc1 exists in the root network namespace"
fi
if ! jq -se \
    --arg source_inode "$SNAPSHOT_MEM_INODE" \
    'length == 8 and all(.[]; .snapshot_mem_inode == $source_inode)' \
    "$TOPOLOGY_NDJSON" >/dev/null; then
    TOPOLOGY_PASS="false"
    hard_failure "snapshot_inode_not_shared" \
        "every clone jail must bind the source snapshot.mem device and inode"
fi
if [[ "$(jq -sr '[.[].process_ns_inode] | unique | length' "$TOPOLOGY_NDJSON")" != "8" ]]; then
    TOPOLOGY_PASS="false"
    hard_failure "namespace_inode_not_unique" "the fleet does not have eight distinct network namespaces"
fi

PHASE="fleet-identity"
log "probing identity, clock, RNG, host/application keys, sudo, and one-shot secrets"
readonly GUEST_EVIDENCE="$PROOF_ROOT/fleet-guest-evidence.tsv"
: >"$GUEST_EVIDENCE"
REMOTE_PROBE="$(cat <<'REMOTE'
set -eu
identity="$(cat /run/rooms/identity)"
epoch="$(date +%s)"
kernel_rng="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
host_key="$(ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub -E sha256 | cut -d ' ' -f2)"
probe=/tmp/rooms-rng-probe
rm -f "$probe" "$probe.pub"
ssh-keygen -q -t ed25519 -N '' -f "$probe"
application_key="$(ssh-keygen -lf "$probe.pub" -E sha256 | cut -d ' ' -f2)"
rm -f "$probe" "$probe.pub"
global_name="$(git config --global --get user.name)"
global_email="$(git config --global --get user.email)"
local_name="$(git -C /workspace/repo config --local --get user.name)"
local_email="$(git -C /workspace/repo config --local --get user.email)"
repo_head="$(git -C /workspace/repo rev-parse HEAD)"
sudo -n /bin/true
sudo_ready=ready
secret_line="$(cat /run/rooms/secrets.env)"
rm -- /run/rooms/secrets.env
[ ! -e /run/rooms/secrets.env ] && [ ! -L /run/rooms/secrets.env ]
secret_consumed=absent
printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
  "$identity" "$epoch" "$kernel_rng" "$host_key" "$application_key" \
  "$global_name" "$global_email" "$local_name" "$local_email" "$repo_head" \
  "$sudo_ready" "$secret_line" "$secret_consumed"
REMOTE
)"
readonly REMOTE_PROBE

IDENTITY_PASS="true"
while IFS=$'\t' read -r room_id index namespace guest; do
    if ! evidence="$(ssh_clone "$namespace" "$guest" "$REMOTE_PROBE" 2>"$LOG_DIR/evidence-$room_id.stderr")"; then
        IDENTITY_PASS="false"
        hard_failure "identity_probe_failed" "guest identity probe failed for $room_id"
        continue
    fi
    printf '%s\t%s\t%s\n' "$room_id" "$index" "$evidence" >>"$GUEST_EVIDENCE"
    IFS=$'\t' read -r \
        identity epoch kernel_rng host_key application_key \
        global_name global_email local_name local_email repo_head \
        sudo_ready secret_line secret_consumed <<<"$evidence"
    if [[ ! "$epoch" =~ ^[0-9]+$ ]]; then
        IDENTITY_PASS="false"
        hard_failure "clone_clock_invalid" "clone $room_id returned a non-numeric epoch"
        continue
    fi
    now="$(date +%s)"
    delta=$((now - epoch))
    if ((delta < 0)); then
        delta=$((-delta))
    fi
    if [[ "$identity" != "$room_id" \
        || "$global_name" != "rooms $room_id" \
        || "$global_email" != "$room_id@rooms.invalid" \
        || "$local_name" != "rooms $room_id" \
        || "$local_email" != "$room_id@rooms.invalid" \
        || "$repo_head" != "$SOURCE_HEAD" \
        || "$sudo_ready" != ready \
        || "$secret_line" != "ROOMS_PHASE2_PROBE=$RESUME_SECRET_VALUE" \
        || "$secret_consumed" != absent \
        || ! "$kernel_rng" =~ ^[0-9a-f]{64}$ \
        || "$host_key" != SHA256:* \
        || "$application_key" != SHA256:* \
        || $delta -gt 5 ]]; then
        IDENTITY_PASS="false"
        hard_failure "clone_hygiene_invalid" \
            "identity/git/clock/RNG/host-key/secret evidence is invalid for $room_id"
    fi
done < <(
    jq -r '.clones[] | [.room_id,.clone_net_index,.namespace,.guest_ip] | @tsv' "$FLEET_JSON"
)

if (( $(wc -l <"$GUEST_EVIDENCE") != 8 )); then
    IDENTITY_PASS="false"
    hard_failure "guest_evidence_incomplete" "did not collect eight guest evidence records"
fi
for column in 5 6 7; do
    if (( $(cut -f"$column" "$GUEST_EVIDENCE" | sort -u | wc -l) != 8 )); then
        IDENTITY_PASS="false"
        hard_failure "clone_randomness_duplicated" \
            "guest evidence column $column is not unique across all eight clones"
    fi
done
if (( $(cut -f12 "$GUEST_EVIDENCE" | sort -u | wc -l) != 1 )) \
    || [[ "$(cut -f12 "$GUEST_EVIDENCE" | head -n 1)" != "$SOURCE_HEAD" ]]; then
    IDENTITY_PASS="false"
    hard_failure "clone_repo_head_drift" \
        "the eight clones did not resume the exact local source HEAD $SOURCE_HEAD"
fi

PHASE="fleet-isolation"
log "probing the cross-clone isolation ring"
ISOLATION_PASS="true"
while IFS=$'\t' read -r room_id index namespace guest; do
    next_index=$((index % 8 + 1))
    sibling_address="172.17.0.$((4 * next_index + 2))"
    isolation_result=""
    if ! isolation_result="$(ssh_clone "$namespace" "$guest" \
        "set -eu
command -v ping >/dev/null || { printf NO_PING; exit 40; }
gateway=\$(ip route show default | awk 'NR == 1 {print \$3}')
[ -n \"\$gateway\" ] || { printf NO_GATEWAY; exit 41; }
sudo -n ping -c 1 -W 1 \"\$gateway\" >/dev/null 2>&1 || { printf CONTROL_FAILED; exit 42; }
if sudo -n ping -c 1 -W 1 $sibling_address >/dev/null 2>&1; then printf REACHABLE; else printf BLOCKED; fi" \
        2>"$LOG_DIR/isolation-$room_id.stderr")"; then
        printf '%s\n' "$isolation_result" >"$LOG_DIR/isolation-$room_id.stdout"
        ISOLATION_PASS="false"
        hard_failure "isolation_probe_failed" \
            "clone $index lacked ping/default-route control or could not reach its same-clone gateway while probing clone $next_index"
        continue
    fi
    printf '%s\n' "$isolation_result" >"$LOG_DIR/isolation-$room_id.stdout"
    if [[ "$isolation_result" == "REACHABLE" ]]; then
        ISOLATION_PASS="false"
        hard_failure "cross_clone_reachable" \
            "clone $index reached clone $next_index namespace address $sibling_address"
    elif [[ "$isolation_result" != "BLOCKED" ]]; then
        ISOLATION_PASS="false"
        hard_failure "isolation_probe_malformed" \
            "clone $index returned an unexpected isolation probe result"
    fi
done < <(
    jq -r '.clones[] | [.room_id,.clone_net_index,.namespace,.guest_ip] | @tsv' "$FLEET_JSON"
)

FROZEN_GUEST="$(jq -er '.clones[0].guest_ip' "$FLEET_JSON")"
readonly FROZEN_GUEST
if direct_ssh "$FROZEN_GUEST" true \
    >"$LOG_DIR/direct-host-ssh.stdout" \
    2>"$LOG_DIR/direct-host-ssh.stderr"; then
    ISOLATION_PASS="false"
    hard_failure "guest_reachable_from_root_namespace" \
        "the duplicate frozen guest IP accepted SSH outside a clone namespace"
fi

PHASE="fleet-teardown"
log "tearing down the exact kept fleet"
kill_batch_exact "$FLEET_JSON" "fleet"
assert_reservation || hard_failure "fleet_reservation_drift" \
    "eight-clone teardown did not return the exact reservation"
if ! assert_batch_owned_paths_absent "$FLEET_JSON"; then
    hard_failure "fleet_owned_path_leak" \
        "kept fleet teardown left a proof-owned room, jail, intent, or claim path"
fi
if ! audit_global_clone_absence; then
    hard_failure "fleet_teardown_leak" "kept fleet teardown left global clone resources"
fi
if ! endpoint_absent; then
    hard_failure "fleet_endpoint_leak" \
        "proof-owned round-trip endpoint was not absent before witnessed workloads"
fi
if ((HARD_FAILURES > 0)); then
    fatal "unsafe_live_gate" \
        "one or more safety assertions failed; refusing the witnessed workload pass"
fi

PHASE="eight-clone-witness"
log "running eight parallel broadcast repo workloads with per-clone witnesses"
readonly WITNESS_ROOT="$ARTIFACT_DIR/witness"
readonly WITNESS_JSON="$PROOF_ROOT/witness-batch.json"
readonly WITNESS_COMMAND='set -eu; id="$(cat /run/rooms/identity)"; command -v timeout >/dev/null; command -v bash >/dev/null; command -v cksum >/dev/null; checksum="$(printf %s "$id" | cksum | awk "{print \$1}")"; port=$((1024 + checksum % 50000)); git -C /workspace/repo fsck --no-progress --strict; printf "room=%s witness_port=%s\n" "$id" "$port"; if timeout 2 bash -c "exec 3<>/dev/tcp/1.1.1.1/$port"; then printf "room=%s witness_port=%s egress=reached\n" "$id" "$port"; exit 97; fi; printf "room=%s witness_port=%s egress=blocked\n" "$id" "$port"; test "$(git config --global --get user.email)" = "${id}@rooms.invalid"'

run_rooms clone "$SNAPSHOT_DIR" \
    --image "$IMAGE" \
    -n 8 \
    --max-pool 8 \
    --command "$WITNESS_COMMAND" \
    --out "$WITNESS_ROOT" \
    --witness \
    --egress none \
    --max-wall 15s \
    --json \
    >"$WITNESS_JSON" \
    2>"$LOG_DIR/witness.stderr"
track_json_ids "$WITNESS_JSON"

WITNESS_PASS="true"
if ! jq -e \
    --arg snapshot_id "$SNAPSHOT_ID" \
    --arg guest "$SNAPSHOT_GUEST" '
    (.clones | length) == 8 and
    all(.clones[];
        .status == "exited" and
        .exit_code == 0 and
        .snapshot_id == $snapshot_id and
        .slot == 1 and
        .guest_ip == $guest) and
    ([.clones[].room_id] | unique | length) == 8 and
    ([.clones[].namespace] | unique | length) == 8 and
    ([.clones[].host_veth] | unique | length) == 8 and
    ([.clones[].clone_net_index] | sort) == [range(1; 9)] and
    ([.clones[].out_dir] | unique | length) == 8
' "$WITNESS_JSON" >/dev/null; then
    WITNESS_PASS="false"
    hard_failure "witness_batch_invalid" "witness workload batch did not report eight clean exits"
fi

readonly WITNESS_MANIFEST="$PROOF_ROOT/witness-manifest.tsv"
: >"$WITNESS_MANIFEST"
while IFS=$'\t' read -r room_id out_dir; do
    resolved_out_dir=""
    resolved_pcap_file=""
    resolved_result_file=""
    resolved_stdout_file=""
    resolved_witness_file=""
    expected_port="$(witness_port_for_room "$room_id")" \
        || fatal "witness_port_invalid" "could not derive the witness port for $room_id"
    witness_file="$out_dir/witness.json"
    pcap_file="$out_dir/witness.pcap"
    result_file="$out_dir/result.json"
    stdout_file="$out_dir/logs/stdout.log"
    witness_ok="true"
    if [[ "$out_dir" != "$WITNESS_ROOT/$room_id" \
        ]] \
        || ! privileged_directory "$out_dir" \
        || ! privileged_nonempty_file "$witness_file" \
        || ! privileged_nonempty_file "$pcap_file" \
        || ! privileged_nonempty_file "$result_file" \
        || ! privileged_nonempty_file "$stdout_file"; then
        witness_ok="false"
    fi
    if [[ "$witness_ok" == "true" ]]; then
        resolved_out_dir="$(sudo -n readlink -f -- "$out_dir")" \
            || witness_ok="false"
        resolved_witness_file="$(sudo -n readlink -f -- "$witness_file")" \
            || witness_ok="false"
        resolved_pcap_file="$(sudo -n readlink -f -- "$pcap_file")" \
            || witness_ok="false"
        resolved_result_file="$(sudo -n readlink -f -- "$result_file")" \
            || witness_ok="false"
        resolved_stdout_file="$(sudo -n readlink -f -- "$stdout_file")" \
            || witness_ok="false"
        if [[ "$resolved_out_dir" != "$out_dir" \
            || "$resolved_witness_file" != "$witness_file" \
            || "$resolved_pcap_file" != "$pcap_file" \
            || "$resolved_result_file" != "$result_file" \
            || "$resolved_stdout_file" != "$stdout_file" ]]; then
            witness_ok="false"
        fi
    fi
    if [[ "$witness_ok" == "true" ]] && ! sudo -n jq -e \
        --argjson expected_port "$expected_port" '
        .schema_version == 2 and
        .tap == "tap-fc1" and
        .capture_complete == true and
        .egress_policy == "none" and
        (.permitted | length) == 0 and
        any(.destinations[]; .ip == "1.1.1.1" and .port == $expected_port and .proto == "tcp") and
        any(.blocked[]; .ip == "1.1.1.1" and .port == $expected_port and .proto == "tcp")
    ' "$witness_file" >/dev/null; then
        witness_ok="false"
    fi
    if [[ "$witness_ok" == "true" ]] \
        && ! sudo -n jq -e \
            '.schema_version == 1 and .status == "succeeded" and .exit_code == 0' \
            "$result_file" >/dev/null; then
        witness_ok="false"
    fi
    if [[ "$witness_ok" == "true" ]] \
        && ! sudo -n grep -Fxq \
            "room=$room_id witness_port=$expected_port egress=blocked" \
            "$stdout_file"; then
        witness_ok="false"
    fi
    if [[ "$witness_ok" == "true" ]] \
        && ! sudo -n tcpdump -nn -r "$pcap_file" -c 1 \
            "dst host 1.1.1.1 and tcp dst port $expected_port" \
            >/dev/null 2>>"$LOG_DIR/tcpdump-read.stderr"; then
        witness_ok="false"
    fi
    if [[ "$witness_ok" != "true" ]]; then
        WITNESS_PASS="false"
        hard_failure "clone_witness_invalid" "witness/output evidence is incomplete for $room_id"
        continue
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$room_id" \
        "$out_dir" \
        "$expected_port" \
        "$(sudo -n stat -Lc '%d:%i' "$pcap_file")" \
        "$(sudo -n stat -Lc %s "$pcap_file")" \
        "$(sudo -n sha256sum "$pcap_file" | awk '{print $1}')" \
        >>"$WITNESS_MANIFEST"
done < <(jq -r '.clones[] | [.room_id,.out_dir] | @tsv' "$WITNESS_JSON")

if (( $(wc -l <"$WITNESS_MANIFEST") != 8 )); then
    WITNESS_PASS="false"
    hard_failure "witness_count_wrong" "did not validate eight independently custodied witness pcaps"
fi
if [[ "$(cut -f3 "$WITNESS_MANIFEST" | sort -u | wc -l)" != "8" ]]; then
    WITNESS_PASS="false"
    hard_failure "witness_port_collision" "room-derived witness tuples were not unique"
fi
if [[ "$(cut -f4 "$WITNESS_MANIFEST" | sort -u | wc -l)" != "8" ]]; then
    WITNESS_PASS="false"
    hard_failure "witness_inode_alias" "witness pcaps do not have eight unique device/inode identities"
fi
if [[ "$(cut -f6 "$WITNESS_MANIFEST" | sort -u | wc -l)" != "8" ]]; then
    WITNESS_PASS="false"
    hard_failure "witness_content_alias" "witness pcaps do not contain eight unique raw captures"
fi

PHASE="final-leak-audit"
log "auditing terminal cleanup and protected canonical state"
FINAL_LEAK_AUDIT_PASS="true"
run_rooms ls --json >"$PROOF_ROOT/final-ls.json" 2>"$LOG_DIR/final-ls.stderr"
if ! jq -e '.rooms | length == 0' "$PROOF_ROOT/final-ls.json" >/dev/null; then
    FINAL_LEAK_AUDIT_PASS="false"
    hard_failure "rooms_not_empty" "proof HOME still lists rooms after command-mode teardown"
fi
if ! assert_terminal_proof_slot; then
    FINAL_LEAK_AUDIT_PASS="false"
    hard_failure "final_reservation_drift" \
        "terminal slot token or digest is not the exact published snapshot reservation"
fi
if ! privileged_paths_absent "$STATE_DIR/restore-intents"; then
    if ! privileged_directory "$STATE_DIR/restore-intents" \
        || sudo -n find "$STATE_DIR/restore-intents" \
            -maxdepth 1 -name '*.json' -print -quit | grep -q .; then
        FINAL_LEAK_AUDIT_PASS="false"
        hard_failure "restore_intent_leak" \
            "restore intent tombstones or an invalid intent path remain"
    fi
fi
if ! privileged_paths_absent "$STATE_DIR/clonenets"; then
    if ! privileged_directory "$STATE_DIR/clonenets" \
        || sudo -n find "$STATE_DIR/clonenets" \
            -maxdepth 1 -type f -print -quit | grep -q .; then
        FINAL_LEAK_AUDIT_PASS="false"
        hard_failure "clonenet_claim_leak" \
            "clone-network claims or an invalid claim path remain"
    fi
fi
if ! privileged_paths_absent "$STATE_DIR/snapshot-intents"; then
    if ! privileged_directory "$STATE_DIR/snapshot-intents" \
        || sudo -n find "$STATE_DIR/snapshot-intents" \
            -maxdepth 1 -name '*.json' -print -quit | grep -q .; then
        FINAL_LEAK_AUDIT_PASS="false"
        hard_failure "snapshot_intent_leak" \
            "snapshot intent tombstones or an invalid intent path remain"
    fi
fi
if ! proof_transients_absent; then
    FINAL_LEAK_AUDIT_PASS="false"
    hard_failure "proof_transient_path_leak" \
        "a proof restore intent, snapshot intent, attestation temp, or clone-network claim remains"
fi
if ! endpoint_absent; then
    FINAL_LEAK_AUDIT_PASS="false"
    hard_failure "proof_endpoint_final_leak" \
        "proof-owned endpoint interface, address, listener, or firewall marker remains"
fi
if ! audit_global_clone_absence; then
    FINAL_LEAK_AUDIT_PASS="false"
    hard_failure "global_resource_leak" "namespace/veth/tap/firewall resources remain"
fi
if pgrep -x firecracker >/dev/null 2>&1 || pgrep -x jailer >/dev/null 2>&1; then
    FINAL_LEAK_AUDIT_PASS="false"
    hard_failure "vmm_process_leak" "a Firecracker or jailer process remains"
fi
if ! sudo -n sha256sum -c --status "$PROOF_ROOT/build-artifacts.sha256"; then
    FINAL_LEAK_AUDIT_PASS="false"
    hard_failure "build_artifact_changed" \
        "the proof binary, fresh rootfs, or copied kernel changed after build verification"
fi
if ! sudo -n sha256sum -c --status "$PROOF_ROOT/snapshot-artifacts.sha256"; then
    FINAL_LEAK_AUDIT_PASS="false"
    hard_failure "snapshot_artifact_changed" \
        "one or more neutral snapshot artifacts changed after publication"
fi
if findmnt -rn -o TARGET | grep -Fq "$STATE_DIR/jailer/"; then
    FINAL_LEAK_AUDIT_PASS="false"
    hard_failure "jail_mount_leak" "a proof jail bind mount remains"
fi
while read -r created_room_id; do
    if ! privileged_paths_absent \
        "$STATE_DIR/$created_room_id" \
        "$STATE_DIR/jailer/firecracker/$created_room_id" \
        "$STATE_DIR/restore-intents/$created_room_id.json" \
        "$STATE_DIR/snapshot-intents/$created_room_id.json"; then
        FINAL_LEAK_AUDIT_PASS="false"
        hard_failure "created_room_path_leak" \
            "managed paths remain for proof-owned room $created_room_id"
    fi
done < <(sort -u "$CREATED_IDS_FILE")
for artifact in snapshot.json snapshot.mem snapshot.vmstate; do
    if ! privileged_nonempty_file "$SNAPSHOT_DIR/$artifact"; then
        FINAL_LEAK_AUDIT_PASS="false"
        hard_failure "snapshot_not_preserved" "$artifact disappeared during the gate"
    fi
done

PHASE="complete"
ROOMS_SUBGATE_COMPLETED="true"
if ((HARD_FAILURES > 0 || PERFORMANCE_FAILURES > 0)); then
    fatal "killer_gate_failed" \
        "killer gate completed with $HARD_FAILURES hard and $PERFORMANCE_FAILURES performance failures"
fi

log "ROOMS SUBGATE PASS: eight clones were workload-ready in ${FLEET_ELAPSED_NS}ns (literal ${LITERAL_LATENCY_REQUIREMENT}; <1000000000ns)"
log "PASS: fleet PSS ${FLEET_PSS_KB}KiB; naive ratio $PSS_SHARING_RATIO"
log "PASS: eight isolated identities, post-reseed RNG consumers, exact repo workloads, and witnesses"
log "PASS: eight observe-mode guests completed the hermetic two-hop return-path proof"
log "$RNG_SCOPE_NOTE"
log "$WORKLOAD_SCOPE_NOTE"
log "FULL PHASE-2 GATE INCOMPLETE: distinct-task /work-driver evidence remains an external consumer gate"
