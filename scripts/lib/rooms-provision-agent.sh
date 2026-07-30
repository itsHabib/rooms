#!/bin/sh
# Credential-free neutral-base provisioning agent.
set -eu

PROVISION_DIR=/run/rooms-provision
BUNDLE="$PROVISION_DIR/repo.bundle"
WARM="$PROVISION_DIR/warm"

fail() {
    printf 'ERR %s\n' "$*" >&2
    exit 1
}

read_frame() {
    expected="$1"
    destination="$2"
    IFS=' ' read -r kind length || fail "missing $expected frame"
    [ "$kind" = "$expected" ] || fail "wanted $expected frame, got $kind"
    case "$length" in
        ''|*[!0-9]*) fail "invalid $expected length" ;;
    esac
    : >"$destination"
    [ "$length" -eq 0 ] || dd bs=1 count="$length" of="$destination" 2>/dev/null
}

credentials_absent() {
    [ ! -e /home/rooms/.git-credentials ] || return 1
    [ ! -e /home/rooms/.netrc ] || return 1
    [ ! -e /home/rooms/.config/gh/hosts.yml ] || return 1
    ! su rooms -s /bin/sh -c \
        'env -i HOME=/home/rooms PATH=/usr/bin:/bin git config --global --get credential.helper' \
        >/dev/null 2>&1
}

clone_bundle() {
    [ -s "$BUNDLE" ] || return 0
    rm -rf /workspace/repo
    chown rooms:rooms "$BUNDLE"
    su rooms -s /bin/sh -c \
        "env -i HOME=/home/rooms USER=rooms LOGNAME=rooms PATH=/usr/local/bin:/usr/bin:/bin \
         git clone '$BUNDLE' /workspace/repo && \
         env -i HOME=/home/rooms USER=rooms LOGNAME=rooms PATH=/usr/local/bin:/usr/bin:/bin \
         git -C /workspace/repo remote remove origin"
}

run_warm() {
    [ -s "$WARM" ] || return 0
    chown rooms:rooms "$WARM"
    chmod 0500 "$WARM"
    su rooms -s /bin/sh -c \
        "exec env -i HOME=/home/rooms USER=rooms LOGNAME=rooms \
         PATH=/usr/local/bin:/usr/bin:/bin ROOMS_NEUTRAL_WARM=1 /bin/sh '$WARM'"
}

ipv6_is_disabled() {
    [ ! -s /proc/net/if_inet6 ] || return 1
    [ ! -e /sys/module/ipv6/parameters/disable ] \
        || grep -Eq '^(1|Y)$' /sys/module/ipv6/parameters/disable
}

no_ssh_surface() {
    ! ps -eo comm= 2>/dev/null | grep -Eq '(^|/)sshd$' || return 1
    for table in /proc/net/tcp /proc/net/tcp6; do
        [ -r "$table" ] || continue
        if awk '$4 == "0A" && $2 ~ /:0016$/ { found=1 } END { exit !found }' "$table"; then
            return 1
        fi
    done
    return 0
}

retained_processes_are_safe() {
    for proc in /proc/[0-9]*; do
        [ -L "$proc/exe" ] || continue
        IFS= read -r comm <"$proc/comm" || return 1
        case "$comm" in
            init|openrc|openrc-run|sh|socat|busybox|rooms-provision*) ;;
            *) printf 'unexpected retained process: %s\n' "$comm" >&2; return 1 ;;
        esac
    done
}

emit_beacon() {
    # This helper has redirected stdio, so it cannot retain the provisioning
    # stream inherited from the service's socat process.
    sleep 1
    printf 'quiesced\n' | socat - VSOCK-CONNECT:2:5002
}

session() {
    IFS= read -r preface || fail "missing protocol preface"
    [ "$preface" = "ROOMS-PROVISION/1" ] || fail "unsupported protocol"
    install -d -m 0700 "$PROVISION_DIR"

    read_frame BUNDLE "$BUNDLE"
    printf 'ACK stage\n'
    clone_bundle
    printf 'ACK clone\n'

    read_frame WARM "$WARM"
    IFS=' ' read -r end end_length || fail "missing END frame"
    [ "$end" = END ] && [ "$end_length" = 0 ] || fail "malformed END frame"
    credentials_absent || fail "configured credential source present"
    run_warm
    printf 'ACK warm\n'

    rm -f "$BUNDLE" "$WARM"
    rmdir "$PROVISION_DIR"
    for service in sshd crond syslog acpid; do
        rc-service "$service" stop >/dev/null 2>&1 || true
    done
    ipv6_is_disabled || fail "IPv6 is not provably disabled"
    no_ssh_surface || fail "SSH process or listener survived quiesce"
    retained_processes_are_safe || fail "unexpected process survived quiesce"
    emit_beacon </dev/null >/dev/null 2>&1 &
}

[ "${1:-}" = session ] || fail "usage: rooms-provision-agent session"
session
