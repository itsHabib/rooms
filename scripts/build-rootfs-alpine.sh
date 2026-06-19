#!/usr/bin/env bash
# Build a rooms agent rootfs ext4 image on Alpine Linux (musl/busybox/openrc).
#
# Produces a small guest image with openssh (key-only login for a non-root
# `rooms` user, AcceptEnv ANTHROPIC_API_KEY), git, ca-certificates, and the
# claude-code native musl binary. DNS resolves out of the box and SSH host keys
# are baked at build time, so the guest reaches sshd within a couple of seconds
# of boot. The agent runs as the unprivileged `rooms` user (uid 1000) because
# claude-code refuses --dangerously-skip-permissions as root.
#
# Usage:
#   sudo ./scripts/build-rootfs-alpine.sh \
#     --out images/agent-alpine.ext4 \
#     --ssh-key ~/.ssh/id_rooms.pub
#
# Pair the image with a virtio-rng kernel at images/vmlinux.bin (see
# scripts/setup-rooms-host.sh). Requires root (loop mount + chroot). See
# scripts/README.md for prereqs and verification.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/rootfs-helpers.sh
source "${SCRIPT_DIR}/lib/rootfs-helpers.sh"

# Pinned inputs. Bump deliberately and re-run the build (the smoke gate at the
# end refuses an image whose claude binary fails to link against musl).
# The minirootfs digest (ALPINE_MINIROOTFS_SHA256, below) is pinned to exactly
# this Alpine release; --alpine-version is rejected unless it matches. Bump both
# together and update scripts/checksums.txt.
ALPINE_PINNED_VERSION="3.21.7"
ALPINE_VERSION="$ALPINE_PINNED_VERSION"
CLAUDE_VERSION="2.1.148-r1"
GUEST_USER="rooms"
GUEST_UID="1000"
OUT="images/agent-alpine.ext4"
SIZE="512M"
SSH_KEY=""
EXTEND=""

ALPINE_CDN="https://dl-cdn.alpinelinux.org/alpine"
# Pinned in scripts/checksums.txt — do not trust the CDN .sha256 sidecar alone.
ALPINE_MINIROOTFS_SHA256="8cba1ea3e8b500ea986a313d8eecf3d5952a2a0d23a69117bb81c023d9ceac05"
CLAUDE_KEY_URL="https://downloads.claude.ai/keys/claude-code.rsa.pub"
CLAUDE_KEY_SHA256="395759c1f7449ef4cdef305a42e820f3c766d6090d142634ebdb049f113168b6"
CLAUDE_APK_REPO="https://downloads.claude.ai/claude-code/apk/stable"

MNT=""
LOOP=""
WORK=""

log()   { printf '\033[1;34m[build-alpine]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[build-alpine]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<EOF
usage: $0 --ssh-key <pubkey-path> [options]

  --out <path>             Output ext4 path (default: ${OUT})
  --ssh-key <path>         Operator SSH public key baked into the rooms user (required)
  --alpine-version <ver>   Alpine release; must match the pinned ${ALPINE_PINNED_VERSION} (the minirootfs sha256 is pinned)
  --claude-version <ver>   claude-code apk version (default: ${CLAUDE_VERSION})
  --size <size>            Image capacity, e.g. 512M (default: ${SIZE})
  --extend <script>        Script run inside the chroot after baseline installs
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out) OUT="${2:?--out requires a value}"; shift 2 ;;
        --ssh-key) SSH_KEY="${2:?--ssh-key requires a value}"; shift 2 ;;
        --alpine-version) ALPINE_VERSION="${2:?--alpine-version requires a value}"; shift 2 ;;
        --claude-version) CLAUDE_VERSION="${2:?--claude-version requires a value}"; shift 2 ;;
        --size) SIZE="${2:?--size requires a value}"; shift 2 ;;
        --extend) EXTEND="${2:?--extend requires a value}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) fatal "unknown argument: $1 (try --help)" ;;
    esac
done

assert_root

[[ -n "$SSH_KEY" ]] || fatal "--ssh-key is required (pubkey-only auth; no password)"
[[ -f "$SSH_KEY" ]] || fatal "ssh public key not found: $SSH_KEY"
[[ -z "$EXTEND" || -f "$EXTEND" ]] || fatal "--extend script not found: $EXTEND"

# The minirootfs sha256 is pinned to one release; a different --alpine-version
# would download a legitimate tarball that fails the hardcoded digest gate.
# Reject early with an actionable message instead of a confusing sha mismatch.
[[ "$ALPINE_VERSION" == "$ALPINE_PINNED_VERSION" ]] \
    || fatal "--alpine-version ${ALPINE_VERSION} unsupported: the minirootfs sha256 is pinned to ${ALPINE_PINNED_VERSION}; to bump, update ALPINE_PINNED_VERSION + ALPINE_MINIROOTFS_SHA256 (and scripts/checksums.txt) together"

# Alpine branch is vMAJOR.MINOR (e.g. 3.21.7 -> v3.21).
ALPINE_BRANCH="v$(printf '%s' "$ALPINE_VERSION" | cut -d. -f1,2)"
TARBALL="alpine-minirootfs-${ALPINE_VERSION}-x86_64.tar.gz"
MIRROR="${ALPINE_CDN}/${ALPINE_BRANCH}/releases/x86_64"

MISSING=()
for cmd in mkfs.ext4 mount umount chroot losetup truncate curl sha256sum tar; do
    command -v "$cmd" >/dev/null 2>&1 || MISSING+=("$cmd")
done
((${#MISSING[@]} == 0)) || fatal "missing tools: ${MISSING[*]}; install with: apt install e2fsprogs util-linux curl coreutils tar"

trap 'rc=$?; cleanup_mount "$MNT" "$LOOP"; [[ -n "$WORK" ]] && rm -rf "$WORK"; exit "$rc"' EXIT
trap 'cleanup_mount "$MNT" "$LOOP"; [[ -n "$WORK" ]] && rm -rf "$WORK"; trap - EXIT; exit 130' INT
trap 'cleanup_mount "$MNT" "$LOOP"; [[ -n "$WORK" ]] && rm -rf "$WORK"; trap - EXIT; exit 143' TERM

OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR"
TMP_OUT="${OUT}.tmp"
[[ -e "$TMP_OUT" ]] && rm -f "$TMP_OUT"

WORK="$(mktemp -d -t rooms-alpine.XXXXXX)"
TARBALL_PATH="${WORK}/${TARBALL}"

log "fetching ${TARBALL}"
curl -fsSL "${MIRROR}/${TARBALL}" -o "$TARBALL_PATH"
log "verifying minirootfs sha256 (pinned for Alpine ${ALPINE_VERSION})"
GOT="$(sha256sum "$TARBALL_PATH" | awk '{print $1}')"
[[ "$GOT" = "$ALPINE_MINIROOTFS_SHA256" ]] \
    || fatal "minirootfs sha256 mismatch for ${TARBALL}: expected ${ALPINE_MINIROOTFS_SHA256}, got ${GOT} (see scripts/checksums.txt)"

log "allocating ${SIZE} sparse image at $TMP_OUT"
# truncate (not fallocate) keeps the file sparse so allocated size tracks
# guest contents, not the full capacity.
truncate -s "$SIZE" "$TMP_OUT"
log "formatting ext4"
mkfs.ext4 -F -L rooms-agent "$TMP_OUT" >/dev/null

MNT="$(mktemp -d -t rooms-alpine-mnt.XXXXXX)"
LOOP="$(losetup -f --show "$TMP_OUT")"
log "mounting $LOOP -> $MNT"
mount "$LOOP" "$MNT"

log "extracting minirootfs"
tar -xzf "$TARBALL_PATH" -C "$MNT" --numeric-owner

mount --bind /dev "$MNT/dev"
mount --bind /proc "$MNT/proc"
mount --bind /sys "$MNT/sys"
# resolv.conf for the apk/key fetch phase; overwritten with the static one below.
cp /etc/resolv.conf "$MNT/etc/resolv.conf"

cat >"$MNT/etc/apk/repositories" <<EOF
${ALPINE_CDN}/${ALPINE_BRANCH}/main
${ALPINE_CDN}/${ALPINE_BRANCH}/community
EOF

log "installing packages + claude-code inside chroot"
# Positional args ($1..$4) feed the quoted heredoc so no host-side expansion
# leaks in; the block runs under busybox ash (bash isn't present until apk add).
chroot "$MNT" /bin/sh -s -- \
    "$CLAUDE_VERSION" "$CLAUDE_KEY_URL" "$CLAUDE_KEY_SHA256" "$CLAUDE_APK_REPO" <<'CHROOT_INSTALL'
set -e
CLAUDE_VERSION="$1"; KEY_URL="$2"; KEY_SHA="$3"; APK_REPO="$4"
apk update
apk add --no-cache \
    alpine-base openrc openssh-server sudo \
    git ca-certificates bash curl \
    libgcc libstdc++ ripgrep
wget -q -O /etc/apk/keys/claude-code.rsa.pub "$KEY_URL"
GOT="$(sha256sum /etc/apk/keys/claude-code.rsa.pub | cut -d' ' -f1)"
[ "$GOT" = "$KEY_SHA" ] || { echo "claude-code apk key sha256 mismatch: got $GOT" >&2; exit 1; }
echo "$APK_REPO" >> /etc/apk/repositories
apk update
apk add --no-cache "claude-code=$CLAUDE_VERSION"
CHROOT_INSTALL

log "installing overlay-init (read-only rootfs + tmpfs overlay at boot)"
install -d -m 0755 "$MNT/mnt" "$MNT/oldroot"
install -m 0755 "${SCRIPT_DIR}/lib/overlay-init.sh" "$MNT/sbin/overlay-init"

log "writing guest config (init, dns, hostname, settings)"
# ttyS0-only inittab — Firecracker never offers a VT, so the six default gettys
# would only burn spawns. openrc drives sysinit/boot/default runlevels.
cat >"$MNT/etc/inittab" <<'EOF'
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default
ttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100
::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
EOF

# Static DNS. No DHCP client runs (kernel ip= owns eth0), so nothing rewrites it.
cat >"$MNT/etc/resolv.conf" <<'EOF'
nameserver 1.1.1.1
nameserver 8.8.8.8
EOF

# Loopback only — the kernel ip= boot arg configures eth0; keep ifupdown off it.
cat >"$MNT/etc/network/interfaces" <<'EOF'
auto lo
iface lo inet loopback
EOF

printf 'rooms-agent\n' >"$MNT/etc/hostname"
if grep -qE '^#?rc_parallel=' "$MNT/etc/rc.conf"; then
    sed -i -E 's/^#?rc_parallel=.*/rc_parallel="YES"/' "$MNT/etc/rc.conf"
else
    printf 'rc_parallel="YES"\n' >>"$MNT/etc/rc.conf"
fi

log "hardening sshd (key-only, no root login, env passthrough)"
SSHD="$MNT/etc/ssh/sshd_config"
set_sshd() {
    local key="$1" val="$2"
    if grep -qE "^${key}[[:space:]]+${val}$" "$SSHD"; then
        return 0
    fi
    if grep -qE "^${key}[[:space:]]" "$SSHD"; then
        sed -i "s|^${key}[[:space:]].*|${key} ${val}|" "$SSHD"
    else
        printf '%s %s\n' "$key" "$val" >>"$SSHD"
    fi
}
set_sshd PermitRootLogin no
set_sshd PubkeyAuthentication yes
set_sshd PasswordAuthentication no
set_sshd UseDNS no
if ! grep -qE '^AcceptEnv[[:space:]].*\bANTHROPIC_API_KEY\b' "$SSHD"; then
    printf 'AcceptEnv ANTHROPIC_API_KEY\n' >>"$SSHD"
fi

log "creating ${GUEST_USER} user + enabling services + baking host keys inside chroot"
chroot "$MNT" /bin/sh -s -- "$GUEST_USER" "$GUEST_UID" <<'CHROOT_CONFIG'
set -e
GUEST_USER="$1"; GUEST_UID="$2"
# Unprivileged agent user (claude-code refuses to skip permissions as root).
addgroup -g "$GUEST_UID" "$GUEST_USER" 2>/dev/null || true
adduser -D -u "$GUEST_UID" -G "$GUEST_USER" -s /bin/bash "$GUEST_USER"
# adduser -D password-locks the account ("!"); Alpine's PAM-less sshd refuses
# even pubkey login for a "!"-locked account, so switch to "*" (disabled
# password, not locked) — matching root's shadow entry, which authed fine.
sed -i "s/^${GUEST_USER}:!/${GUEST_USER}:*/" /etc/shadow
echo "$GUEST_USER ALL=(ALL) NOPASSWD: ALL" > "/etc/sudoers.d/$GUEST_USER"
chmod 440 "/etc/sudoers.d/$GUEST_USER"
# Pre-create the agent workspace owned by the guest user so repos + artifacts
# land without needing root inside the guest.
mkdir -p /workspace
chown "$GUEST_USER:$GUEST_USER" /workspace
chmod 755 /workspace
# sysinit: device + kernel-message handling.
rc-update add devfs sysinit
rc-update add dmesg sysinit
# boot: pseudo-filesystems, hostname, /run scaffolding.
rc-update add procfs boot
rc-update add sysfs boot
rc-update add bootmisc boot
rc-update add hostname boot
# default: the service rooms actually connects to.
rc-update add sshd default
# Never run ifupdown/DHCP — the kernel ip= arg already configured eth0.
rc-update del networking boot 2>/dev/null || true
rc-update del hwclock boot 2>/dev/null || true
rc-update del modules boot 2>/dev/null || true
# Host keys at build time — first-boot keygen would block on guest entropy.
ssh-keygen -A
install -d -m 0711 /var/empty
# Populate the CA bundle git + claude TLS rely on.
update-ca-certificates 2>/dev/null || true
test -s /etc/ssl/certs/ca-certificates.crt || { echo "ca-certificates bundle empty" >&2; exit 1; }
# Trim build leftovers (no-op-ish since apk add --no-cache, but drop the index).
rm -rf /var/cache/apk/* /usr/share/man/* /usr/share/doc/* 2>/dev/null || true
CHROOT_CONFIG

log "installing ${GUEST_USER} authorized_keys + claude settings from host"
install -d -m 700 -o "$GUEST_UID" -g "$GUEST_UID" "$MNT/home/$GUEST_USER/.ssh"
install -m 600 -o "$GUEST_UID" -g "$GUEST_UID" "$SSH_KEY" "$MNT/home/$GUEST_USER/.ssh/authorized_keys"
install -d -m 700 -o "$GUEST_UID" -g "$GUEST_UID" "$MNT/home/$GUEST_USER/.claude"
cat >"$MNT/home/$GUEST_USER/.claude/settings.json" <<'EOF'
{ "env": { "USE_BUILTIN_RIPGREP": "0", "DISABLE_AUTOUPDATER": "1" } }
EOF
chown "$GUEST_UID:$GUEST_UID" "$MNT/home/$GUEST_USER/.claude/settings.json"

log "smoke gate: claude --version inside the image"
SMOKE="$(chroot "$MNT" /bin/sh -c 'USE_BUILTIN_RIPGREP=0 claude --version' 2>&1)" \
    || fatal "claude --version failed in image: $SMOKE"
if printf '%s' "$SMOKE" | grep -qiE 'symbol not found|Error relocating'; then
    fatal "glibc symbol leaked into the musl claude binary: $SMOKE"
fi
log "claude ok in image: $SMOKE"

if [[ -n "$EXTEND" ]]; then
    EXT_DIR="$(dirname "$EXTEND")"
    EXT_BASENAME="$(basename "$EXTEND")"
    log "staging --extend assets from ${EXT_DIR} into chroot /tmp"
    for asset in "$EXT_DIR"/*; do
        [[ -f "$asset" ]] || continue
        [[ "$(basename "$asset")" == "$EXT_BASENAME" ]] && continue
        install -m 0644 "$asset" "$MNT/tmp/$(basename "$asset")"
    done
    log "running extension script inside chroot: $EXTEND"
    install -m 0755 "$EXTEND" "$MNT/tmp/$EXT_BASENAME"
    chroot "$MNT" "/tmp/$EXT_BASENAME"
    rm -f "$MNT/tmp/$EXT_BASENAME"
    for asset in "$EXT_DIR"/*; do
        [[ -f "$asset" ]] || continue
        [[ "$(basename "$asset")" == "$EXT_BASENAME" ]] && continue
        rm -f "$MNT/tmp/$(basename "$asset")"
    done
fi

log "syncing and unmounting"
sync
umount "$MNT/sys"
umount "$MNT/proc"
umount "$MNT/dev"
umount "$MNT"
rmdir "$MNT"
MNT=""
losetup -d "$LOOP"
LOOP=""

[[ -f "$OUT" ]] && rm -f "$OUT"
mv "$TMP_OUT" "$OUT"

DIGEST="$(sha256sum "$OUT" | awk '{print $1}')"
ALLOC="$(du -h "$OUT" | awk '{print $1}')"
log "done: $OUT (${ALLOC} allocated, ${SIZE} capacity)"
log "sha256: $DIGEST"
log "guest user: ${GUEST_USER} (uid ${GUEST_UID}); ssh as ${GUEST_USER}@<guest-ip>"
log "smoke test: ./scripts/test-rootfs-alpine.sh $OUT"
