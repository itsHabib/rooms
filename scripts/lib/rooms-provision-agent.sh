#!/bin/sh
# Credential-free neutral-base provisioning agent.
set -eu

PROVISION_DIR=/run/rooms-provision
BUNDLE="$PROVISION_DIR/repo.bundle"
WARM="$PROVISION_DIR/warm"
PROCESS_BASELINE="$PROVISION_DIR/processes.before-warm"

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
    if [ "$length" -ne 0 ]; then
        # Alpine's `head` is the BusyBox applet: `-c` reads exactly this many
        # bytes from the stream without consuming the following frame header.
        head -c "$length" >"$destination"
    fi
    actual="$(wc -c <"$destination")"
    [ "$actual" -eq "$length" ] || fail "short $expected frame: wanted $length bytes, got $actual"
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

process_identity() {
    proc="$1"
    pid="${proc##*/}"
    IFS= read -r stat <"$proc/stat" || return 1
    rest="${stat##*) }"
    [ "$rest" != "$stat" ] || return 1
    set -- $rest
    [ "$#" -ge 20 ] || return 1
    shift 19
    printf '%s:%s\n' "$pid" "$1"
}

capture_process_baseline() {
    baseline="$1"
    : >"$baseline"
    for proc in /proc/[0-9]*; do
        [ -L "$proc/exe" ] || continue
        process_identity "$proc" >>"$baseline" || true
    done
}

no_post_warm_processes() {
    baseline="$1"
    for proc in /proc/[0-9]*; do
        [ -L "$proc/exe" ] || continue
        identity="$(process_identity "$proc")" || continue
        grep -Fxq "$identity" "$baseline" && continue
        IFS= read -r comm <"$proc/comm" || comm=unknown
        printf 'post-warm process survived: %s (%s)\n' "$comm" "$identity" >&2
        return 1
    done
}

emit_beacon() {
    # The caller replaces all three stdio descriptors before forking this
    # helper, so it cannot retain the provisioning stream. The host binds this
    # listener before boot but accepts only after provisioning EOF; an early
    # connect queues safely and preserves that structural ordering.
    printf 'quiesced\n' | socat - VSOCK-CONNECT:2:5002
}

session() {
    IFS= read -r preface || fail "missing protocol preface"
    [ "$preface" = "ROOMS-PROVISION/1" ] || fail "unsupported protocol"
    install -d -m 0700 "$PROVISION_DIR"
    capture_process_baseline "$PROCESS_BASELINE"

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
    for service in sshd crond syslog acpid; do
        rc-service "$service" stop >/dev/null 2>&1 || true
    done
    ipv6_is_disabled || fail "IPv6 is not provably disabled"
    no_ssh_surface || fail "SSH process or listener survived quiesce"
    no_post_warm_processes "$PROCESS_BASELINE" || fail "warm descendant survived quiesce"
    rm -f "$PROCESS_BASELINE"
    rmdir "$PROVISION_DIR"
    emit_beacon </dev/null >/dev/null 2>&1 &
}

if [ "${ROOMS_AGENT_LIBRARY_ONLY:-0}" = 1 ]; then
    return 0 2>/dev/null || exit 0
fi

[ "${1:-}" = session ] || fail "usage: rooms-provision-agent session"
session
