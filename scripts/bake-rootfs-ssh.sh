#!/usr/bin/env bash
set -euo pipefail

# Refuse to run as root: HOME-derived defaults (ROOTFS, KEY_PATH) would
# point at /root instead of the operator's home when invoked via `sudo
# bash bake-rootfs-ssh.sh`, baking the wrong pubkey. The script invokes
# `sudo` internally for the ops that need it.
if [[ "$EUID" -eq 0 ]]; then
    printf '\033[1;31m[bake-rootfs-ssh]\033[0m do not run as root / under sudo. Run as your regular user; the script will sudo internally where needed.\n' >&2
    exit 1
fi

ROOTFS="${1:-$HOME/rooms/images/rootfs.ext4}"
KEY_PATH="${KEY_PATH:-$HOME/.ssh/id_rooms}"
PUB_PATH="${KEY_PATH}.pub"

# Declare early so the trap (registered before any losetup/mount) can
# reference them without tripping `set -u`.
MNT=""
LOOP=""

log()   { printf '\033[1;34m[bake-rootfs-ssh]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[bake-rootfs-ssh]\033[0m %s\n' "$*" >&2; exit 1; }

# Cleanup body. Idempotent — safe to invoke from EXIT, INT, or TERM.
# Does NOT exit; the caller is responsible for the final exit code.
cleanup() {
    if [[ -n "$MNT" ]] && mountpoint -q "$MNT"; then
        sudo umount "$MNT" || log "warn: umount $MNT failed (may already be unmounted)"
    fi
    # losetup probe needs sudo on Ubuntu (otherwise EPERM → skip → loop leak).
    if [[ -n "$LOOP" ]] && sudo losetup "$LOOP" >/dev/null 2>&1; then
        sudo losetup -d "$LOOP" || log "warn: losetup -d $LOOP failed"
    fi
    if [[ -n "$MNT" && -d "$MNT" ]]; then
        rmdir "$MNT" 2>/dev/null || true
    fi
}
# Separate traps so signal exits preserve their conventional code (128 + signum).
# Without this, $? inside the trap is whatever the last completed command
# returned — often 0 during a sleep — so a Ctrl-C'd bake exits 0 and looks like
# success to callers.
trap 'rc=$?; cleanup; exit "$rc"' EXIT
trap 'cleanup; trap - EXIT; exit 130' INT   # 128 + SIGINT(2)
trap 'cleanup; trap - EXIT; exit 143' TERM  # 128 + SIGTERM(15)

# 1. Validate prereqs (runtime only — shellcheck is lint-time, gated separately
# by `make check` / CI, not enforced here)
MISSING=()
for cmd in sudo mount mountpoint losetup ssh ssh-keygen sed grep tee e2fsck awk; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        MISSING+=("$cmd")
    fi
done
if ((${#MISSING[@]} > 0)); then
    fatal "missing required tools: ${MISSING[*]}; install with: sudo apt install util-linux openssh-client e2fsprogs gawk"
fi

# 2. Argument validation
if [[ ! -f "$ROOTFS" ]]; then
    fatal "rootfs not found: $ROOTFS"
fi
if [[ ! -w "$ROOTFS" ]]; then
    fatal "rootfs not writable: $ROOTFS"
fi
if loop_attached=$(sudo losetup -j "$ROOTFS" 2>/dev/null) && [[ -n "$loop_attached" ]]; then
    # losetup -j output: "/dev/loop0: [...] (file)" — name is everything
    # before the first colon, NOT $2 of the colon split (which is metadata).
    loop_name=$(printf '%s\n' "$loop_attached" | awk -F':' 'NR==1 {print $1}')
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
elif [[ -f "$KEY_PATH" && ! -f "$PUB_PATH" ]]; then
    # Recovery: private key present but public key missing (e.g. .pub deleted
    # or wasn't copied alongside the private). Regenerate the .pub from the
    # private — ssh-keygen -y deterministically derives it. Avoids the
    # overwrite-existing-private-key prompt that would otherwise EPERM in
    # non-interactive runs.
    log "private key at $KEY_PATH but public key missing; regenerating $PUB_PATH from private"
    ssh-keygen -y -f "$KEY_PATH" > "$PUB_PATH"
    chmod 644 "$PUB_PATH"
else
    # Ensure parent dir exists — ssh-keygen errors out if it doesn't
    # (common on fresh hosts that have never run ssh-keygen).
    KEY_DIR="$(dirname "$KEY_PATH")"
    if [[ ! -d "$KEY_DIR" ]]; then
        log "creating $KEY_DIR"
        mkdir -p "$KEY_DIR"
        chmod 700 "$KEY_DIR"
    fi
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
set_directive AcceptEnv ANTHROPIC_API_KEY

# 7b. Replace the quickstart rootfs's stale /etc/resolv.conf.
# The bionic quickstart image was built on AWS EC2 and ships with a
# `nameserver 172.31.0.2` entry that's unreachable from our Hyper-V host;
# curl in the guest fails with "couldn't resolve host" until this is
# overwritten with a real public resolver. Goes away when we control the
# rootfs at build time.
RESOLV="$MNT/etc/resolv.conf"
log "writing /etc/resolv.conf (overriding AWS leftovers)"
sudo tee "$RESOLV" >/dev/null <<EOF
nameserver 1.1.1.1
nameserver 8.8.8.8
EOF
sudo chmod 644 "$RESOLV"

# 8. Sync + unmount + fsck
sync
sudo umount "$MNT"
# rmdir the tempdir BEFORE clearing MNT — once we clear MNT the EXIT trap
# won't know to clean it up, so we'd leak the directory on the success path.
rmdir "$MNT"
MNT=""
sudo e2fsck -fy "$LOOP"
sudo losetup -d "$LOOP"
LOOP=""

# 9. Final logging
log "done."
log "    pubkey baked into:  $ROOTFS"
log "    private key:        $KEY_PATH"
log "    verify after boot:  ssh -i $KEY_PATH -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null root@172.16.0.2 'uname -a'"
log "    env passthrough:    set ANTHROPIC_API_KEY before invoking rooms (SendEnv plumbs it to the guest)"
