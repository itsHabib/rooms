#!/usr/bin/env bash
# Build a rooms v0 Ubuntu rootfs ext4 image via debootstrap.
#
# Usage:
#   sudo ./scripts/build-rootfs.sh \
#     --suite noble \
#     --size 4G \
#     --out images/node-dev.ext4 \
#     --ssh-key ~/.ssh/id_rooms.pub
#
# Requires root. See scripts/README.md for prereqs and verification.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/rootfs-helpers.sh
source "${SCRIPT_DIR}/lib/rootfs-helpers.sh"

SUITE="noble"
SIZE="4G"
OUT="images/node-dev.ext4"
SSH_KEY=""
EXTEND=""
# Pinned 2026-05; override with --node-source if NodeSource changes upstream.
NODE_SOURCE_URL="https://deb.nodesource.com/setup_20.x"

MNT=""
LOOP=""

log()   { printf '\033[1;34m[build-rootfs]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[build-rootfs]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<EOF
usage: $0 --ssh-key <pubkey-path> [options]

  --suite <codename>     Ubuntu suite (default: noble)
  --size <size>          Image capacity, e.g. 4G (default: 4G)
  --out <path>           Output ext4 path (default: images/node-dev.ext4)
  --ssh-key <path>       Operator SSH public key for the rooms user (required)
  --extend <script>      Optional bash script to run inside the chroot after baseline installs
  --node-source <url>    NodeSource setup script URL (default: setup_20.x)
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --suite) SUITE="${2:?--suite requires a value}"; shift 2 ;;
        --size) SIZE="${2:?--size requires a value}"; shift 2 ;;
        --out) OUT="${2:?--out requires a value}"; shift 2 ;;
        --ssh-key) SSH_KEY="${2:?--ssh-key requires a value}"; shift 2 ;;
        --extend) EXTEND="${2:?--extend requires a value}"; shift 2 ;;
        --node-source) NODE_SOURCE_URL="${2:?--node-source requires a value}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) fatal "unknown argument: $1 (try --help)" ;;
    esac
done

assert_root

if [[ -z "$SSH_KEY" ]]; then
    fatal "--ssh-key is required (pubkey-only auth; no default password)"
fi
if [[ ! -f "$SSH_KEY" ]]; then
    fatal "ssh public key not found: $SSH_KEY"
fi

trap 'rc=$?; cleanup_mount "$MNT" "$LOOP"; exit "$rc"' EXIT
trap 'cleanup_mount "$MNT" "$LOOP"; trap - EXIT; exit 130' INT
trap 'cleanup_mount "$MNT" "$LOOP"; trap - EXIT; exit 143' TERM

MISSING=()
for cmd in debootstrap mkfs.ext4 mount umount chroot losetup fallocate curl sha256sum; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        MISSING+=("$cmd")
    fi
done
if ((${#MISSING[@]} > 0)); then
    fatal "missing required tools: ${MISSING[*]}; install with: apt install debootstrap e2fsprogs util-linux curl"
fi

if [[ -n "$EXTEND" && ! -f "$EXTEND" ]]; then
    fatal "--extend script not found: $EXTEND"
fi

OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR"
TMP_OUT="${OUT}.tmp"
if [[ -e "$TMP_OUT" ]]; then
    rm -f "$TMP_OUT"
fi

log "allocating ${SIZE} sparse image at $TMP_OUT"
# `truncate -s` creates an unwritten (sparse) file; `fallocate -l` would
# reserve the full capacity on disk immediately, so a 4G default rootfs
# would consume 4G on the host regardless of guest contents and the
# smoke test's `du --block-size=1` allocated-size guard would always
# trip.
truncate -s "$SIZE" "$TMP_OUT"

log "formatting ext4"
mkfs.ext4 -F -L rooms-rootfs "$TMP_OUT" >/dev/null

MNT="$(mktemp -d -t rooms-rootfs.XXXXXX)"
log "loop-attaching $TMP_OUT"
LOOP="$(losetup -f --show "$TMP_OUT")"
log "mounting $LOOP -> $MNT"
mount "$LOOP" "$MNT"

log "debootstrap --variant=minbase $SUITE (this may take a few minutes)"
debootstrap --variant=minbase "$SUITE" "$MNT" https://archive.ubuntu.com/ubuntu/

pin_chroot_apt "$MNT"
cp /etc/resolv.conf "$MNT/etc/resolv.conf"

cat >"$MNT/etc/apt/sources.list" <<EOF
deb https://archive.ubuntu.com/ubuntu/ ${SUITE} main universe
deb https://archive.ubuntu.com/ubuntu/ ${SUITE}-updates main universe
deb https://security.ubuntu.com/ubuntu/ ${SUITE}-security main universe
EOF

mount --bind /dev "$MNT/dev"
mount --bind /proc "$MNT/proc"
mount --bind /sys "$MNT/sys"
mount -t devpts devpts "$MNT/dev/pts"

log "installing baseline packages inside chroot"
chroot "$MNT" /bin/bash -euo pipefail <<'CHROOT_BASE'
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y \
    git openssh-server curl ca-certificates gnupg iproute2 iputils-ping sudo
CHROOT_BASE

log "configuring rooms user, sshd, and tooling inside chroot"
# shellcheck disable=SC2016
chroot "$MNT" /bin/bash -euo pipefail <<CHROOT_CONFIG
export DEBIAN_FRONTEND=noninteractive

# Node.js 20 via NodeSource (URL passed from host builder).
curl -fsSL '${NODE_SOURCE_URL}' | bash -
apt-get install -y nodejs
npm install -g @anthropic-ai/claude-code

if ! id -u rooms >/dev/null 2>&1; then
    useradd -m -u 1000 -s /bin/bash rooms
fi
echo 'rooms ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/rooms
chmod 440 /etc/sudoers.d/rooms

SSHD=/etc/ssh/sshd_config
set_sshd() {
    local key="\$1" val="\$2"
    if grep -qE "^\${key}[[:space:]]+\${val}\$" "\$SSHD"; then
        return 0
    fi
    if grep -qE "^\${key}[[:space:]]" "\$SSHD"; then
        sed -i "s|^\${key}[[:space:]].*|\${key} \${val}|" "\$SSHD"
    else
        echo "\${key} \${val}" >> "\$SSHD"
    fi
}
set_sshd PermitRootLogin no
set_sshd PubkeyAuthentication yes
set_sshd PasswordAuthentication no
for env_var in ANTHROPIC_API_KEY CLAUDE_CODE_OAUTH_TOKEN ANTHROPIC_AUTH_TOKEN; do
    if ! grep -qE "^AcceptEnv[[:space:]].*\\b\${env_var}\\b" "\$SSHD"; then
        echo "AcceptEnv \${env_var}" >> "\$SSHD"
    fi
done

mkdir -p /etc/systemd/system/multi-user.target.wants
ln -sf /lib/systemd/system/ssh.service /etc/systemd/system/multi-user.target.wants/ssh.service

echo 'rooms-guest' > /etc/hostname

tee /etc/resolv.conf >/dev/null <<'RESOLV'
nameserver 1.1.1.1
nameserver 8.8.8.8
RESOLV

find / -xdev -name '*.log' -delete 2>/dev/null || true
: > /var/log/dpkg.log 2>/dev/null || true
find /var/log/apt -type f -exec truncate -s 0 {} + 2>/dev/null || true
apt-get clean
rm -rf /var/cache/apt/archives/*.deb
find /var/cache -mindepth 1 -delete 2>/dev/null || true
CHROOT_CONFIG

log "installing SSH authorized_keys for rooms user"
install -d -m 700 -o 1000 -g 1000 "$MNT/home/rooms/.ssh"
install -m 600 -o 1000 -g 1000 "$SSH_KEY" "$MNT/home/rooms/.ssh/authorized_keys"

if [[ -n "$EXTEND" ]]; then
    EXT_BASENAME="$(basename "$EXTEND")"
    log "running extension script inside chroot: $EXTEND"
    install -m 0755 "$EXTEND" "$MNT/tmp/$EXT_BASENAME"
    chroot "$MNT" "/tmp/$EXT_BASENAME"
    rm -f "$MNT/tmp/$EXT_BASENAME"
fi

log "syncing and unmounting"
sync
umount "$MNT/dev/pts"
umount "$MNT/sys"
umount "$MNT/proc"
umount "$MNT/dev"
umount "$MNT"
rmdir "$MNT"
MNT=""
losetup -d "$LOOP"
LOOP=""

if [[ -f "$OUT" ]]; then
    rm -f "$OUT"
fi
mv "$TMP_OUT" "$OUT"

DIGEST="$(sha256sum "$OUT" | awk '{print $1}')"
log "done: $OUT"
log "sha256: $DIGEST"
log "verify with: sha256sum $OUT"
log "smoke test: sudo ./scripts/test-rootfs.sh $OUT"
