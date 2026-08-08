#!/bin/sh
# Post-restore hygiene agent — the guest half of the resume nudge.
#
# Runs as a boot-time poll loop baked into the neutral base (so it is in the
# pre-warm process baseline and survives quiesce). No connection is held
# across a snapshot: every attempt is a fresh bounded VSOCK connect that
# fails fast in an ordinary base (the host binds the 5003 listener only in a
# restored room's jail). After a restore the loop reconnects, applies the
# hygiene nudge, and ACKs only once every step has succeeded.
set -eu

RESUME_DIR=/run/rooms
SECRETS_DIR=/run/rooms-secrets

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
        head -c "$length" >"$destination"
    fi
    actual="$(wc -c <"$destination")"
    [ "$actual" -eq "$length" ] || fail "short $expected frame: wanted $length bytes, got $actual"
}

stage_secrets() {
    source_file="$1"
    if [ ! -s "$source_file" ]; then
        rm -f "$source_file"
        return 0
    fi
    install -d -m 0700 -o rooms -g rooms "$SECRETS_DIR"
    install -m 0400 -o rooms -g rooms "$source_file" "$SECRETS_DIR/env"
    rm -f "$source_file"
}

fresh_ssh_host_keys() {
    rm -f /etc/ssh/ssh_host_*
    ssh-keygen -A >/dev/null 2>&1 || fail "ssh-keygen -A failed"
}

session() {
    printf 'ROOMS-RESUME/1\n'
    IFS=' ' read -r kind room_id || fail "missing IDENTITY line"
    [ "$kind" = IDENTITY ] || fail "wanted IDENTITY, got $kind"
    IFS=' ' read -r kind epoch || fail "missing CLOCK line"
    [ "$kind" = CLOCK ] || fail "wanted CLOCK, got $kind"
    case "$epoch" in
        ''|*[!0-9]*) fail "invalid CLOCK epoch" ;;
    esac

    install -d -m 0700 "$RESUME_DIR"
    read_frame ENTROPY "$RESUME_DIR/.entropy"
    read_frame SECRETS "$RESUME_DIR/.secrets"
    IFS=' ' read -r end end_length || fail "missing END frame"
    [ "$end" = END ] && [ "$end_length" = 0 ] || fail "malformed END frame"

    # Reseed first: everything after (host keys) must draw post-divergence.
    cat "$RESUME_DIR/.entropy" >/dev/urandom || fail "cannot reseed /dev/urandom"
    rm -f "$RESUME_DIR/.entropy"
    date -u -s "@$epoch" >/dev/null 2>&1 || fail "cannot step clock"
    printf '%s\n' "$room_id" >"$RESUME_DIR/identity"
    stage_secrets "$RESUME_DIR/.secrets"
    fresh_ssh_host_keys
    rc-service sshd restart >/dev/null 2>&1 \
        || rc-service sshd start >/dev/null 2>&1 \
        || fail "cannot start sshd"

    printf 'ACK resume\n'
}

loop() {
    while :; do
        # -T bounds a wedged session; a refused connect (no listener — the
        # ordinary base case) exits socat immediately and the loop idles.
        socat -T 60 VSOCK-CONNECT:2:5003 EXEC:'/sbin/rooms-resume-agent session' \
            >/dev/null 2>&1 || true
        sleep 2
    done
}

case "${1:-}" in
    session) session ;;
    loop) loop ;;
    *) fail "usage: rooms-resume-agent {session|loop}" ;;
esac
