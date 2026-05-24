#!/usr/bin/env bash
set -euo pipefail

ROOTFS="${1:-$HOME/rooms/images/rootfs.ext4}"
KEY_PATH="${KEY_PATH:-$HOME/.ssh/id_rooms}"
PUB_PATH="${KEY_PATH}.pub"

# Declare early so the trap (registered before any losetup/mount) can
# reference them without tripping `set -u`.
MNT=""
LOOP=""

log()   { printf '\033[1;34m[bake-rootfs-ssh]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[bake-rootfs-ssh]\033[0m %s\n' "$*" >&2; exit 1; }

cleanup() {
    local code=$?
    if [[ -n "$MNT" ]] && mountpoint -q "$MNT"; then
        sudo umount "$MNT" || log "warn: umount $MNT failed (may already be unmounted)"
    fi
    if [[ -n "$LOOP" ]] && losetup "$LOOP" >/dev/null 2>&1; then
        sudo losetup -d "$LOOP" || log "warn: losetup -d $LOOP failed"
    fi
    if [[ -n "$MNT" && -d "$MNT" ]]; then
        rmdir "$MNT" 2>/dev/null || true
    fi
    exit "$code"
}
trap cleanup EXIT INT TERM

# 1. Validate prereqs
MISSING=()
for cmd in sudo mount mountpoint losetup ssh-keygen sed grep tee e2fsck shellcheck; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        MISSING+=("$cmd")
    fi
done
if ((${#MISSING[@]} > 0)); then
    fatal "missing required tools: ${MISSING[*]}; install with: sudo apt install util-linux openssh-client e2fsprogs shellcheck"
fi

# 2. Argument validation
if [[ ! -f "$ROOTFS" ]]; then
    fatal "rootfs not found: $ROOTFS"
fi
if [[ ! -w "$ROOTFS" ]]; then
    fatal "rootfs not writable: $ROOTFS"
fi
if loop_attached=$(losetup -j "$ROOTFS" 2>/dev/null) && [[ -n "$loop_attached" ]]; then
    loop_name=$(printf '%s\n' "$loop_attached" | awk -F': ' 'NR==1 {print $2}' | awk '{print $1}')
    fatal "rootfs already attached to loop device ${loop_name:-unknown}; detach first: sudo losetup -d ${loop_name:-<device>}"
fi

# 3. Preflight safety check
printf '[bake-rootfs-ssh] WARNING: any microVM using this rootfs MUST be shut down\n' >&2
printf '                  before bake. Mounting a live RW ext4 from another writer\n' >&2
printf '                  corrupts it. Press Ctrl-C now if a VM is running; otherwise\n' >&2
printf '                  the script will continue in 5 seconds...\n' >&2
sleep 5

# 4. Host-side SSH key
if [[ -f "$KEY_PATH" && -f "$PUB_PATH" ]]; then
    log "reusing existing keypair at $KEY_PATH (created $(stat -c %y "$KEY_PATH"))"
else
    log "generating ed25519 keypair at $KEY_PATH"
    ssh-keygen -t ed25519 -N "" -f "$KEY_PATH" -C "rooms-microvm" >/dev/null
fi

# 5. Loop-mount the rootfs
MNT="$(mktemp -d -t rooms-bake.XXXXXX)"
log "loop-attaching $ROOTFS"
LOOP="$(sudo losetup -f --show "$ROOTFS")"
log "mounting $LOOP -> $MNT"
sudo mount "$LOOP" "$MNT"

# 6. Bake the key into the mounted rootfs (NO chroot)
log "preparing /root/.ssh in rootfs"
sudo mkdir -p "$MNT/root/.ssh"
sudo chown 0:0 "$MNT/root/.ssh"
sudo chmod 700 "$MNT/root/.ssh"

AK="$MNT/root/.ssh/authorized_keys"
sudo touch "$AK"
sudo chown 0:0 "$AK"
sudo chmod 600 "$AK"

PUBKEY="$(cat "$PUB_PATH")"
if sudo grep -qxF "$PUBKEY" "$AK"; then
    log "pubkey already present in authorized_keys"
else
    log "appending pubkey to authorized_keys"
    echo "$PUBKEY" | sudo tee -a "$AK" >/dev/null
fi

# 7. Configure sshd (idempotent, handles bionic's commented defaults)
CONFIG="$MNT/etc/ssh/sshd_config"

set_directive() {
    local dir="$1" val="$2"
    if sudo grep -qE "^${dir}[[:space:]]+${val}\$" "$CONFIG"; then
        log "$dir already = $val"
    elif sudo grep -qE "^${dir}[[:space:]]" "$CONFIG"; then
        log "$dir present with wrong value; replacing"
        sudo sed -i.bak.rooms "s|^${dir}[[:space:]].*|${dir} ${val}|" "$CONFIG"
    else
        log "$dir missing or commented; appending"
        echo "${dir} ${val}" | sudo tee -a "$CONFIG" >/dev/null
    fi
}

set_directive PermitRootLogin yes
set_directive PubkeyAuthentication yes
set_directive PasswordAuthentication no

# 8. Sync + unmount + fsck
sync
sudo umount "$MNT"
MNT=""
sudo e2fsck -fy "$LOOP"
sudo losetup -d "$LOOP"
LOOP=""

# 9. Final logging
log "done."
log "    pubkey baked into:  $ROOTFS"
log "    private key:        $KEY_PATH"
log "    verify after boot:  ssh -i $KEY_PATH -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null root@172.16.0.2 'uname -a'"
