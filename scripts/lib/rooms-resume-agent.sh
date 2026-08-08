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

# Stream a progress step to the host over the protocol stream (stdout). A
# snapshot-resumed guest's serial console is detached, so this is the host's
# only visibility into hygiene; the host logs each STEP and gates on the
# terminal ACK.
step() {
    printf 'STEP %s\n' "$*"
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
        # dd bs=1 count=N reads EXACTLY N bytes — one read(2) per byte, so it
        # never consumes into the next frame's header. `head -c N` on a socket
        # over-reads (a single large read() grabs the following frame bytes and
        # discards them), which silently desyncs the stream.
        dd bs=1 count="$length" >"$destination" 2>/dev/null
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

# Step the clock from an epoch, tolerating busybox/coreutils date differences.
step_clock() {
    epoch="$1"
    date -u -s "@$epoch" >/dev/null 2>&1 && return 0
    stamp="$(date -u -d "@$epoch" '+%Y-%m-%d %H:%M:%S' 2>/dev/null)" || return 1
    date -u -s "$stamp" >/dev/null 2>&1
}

# Start sshd directly rather than through openrc: a snapshot-resumed openrc
# supervisor can stall, and the daemon is all we need for the SSH re-probe.
# sshd was stopped at quiesce, so the openrc fallbacks try `start` before
# `restart` (restart on a stopped service is the confusing-log case).
start_sshd() {
    /usr/sbin/sshd 2>/dev/null && return 0
    rc-service sshd start >/dev/null 2>&1 && return 0
    rc-service sshd restart >/dev/null 2>&1
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

    # 0755 so an unprivileged workload can read its own room identity below;
    # the transient entropy/secrets frames are removed right after use, and
    # real secrets live in the 0700 SECRETS_DIR.
    install -d -m 0755 "$RESUME_DIR"
    read_frame ENTROPY "$RESUME_DIR/.entropy"
    read_frame SECRETS "$RESUME_DIR/.secrets"
    IFS=' ' read -r end end_length || fail "missing END frame"
    [ "$end" = END ] && [ "$end_length" = 0 ] || fail "malformed END frame"

    # Reseed first: everything after (host keys) must draw post-divergence.
    cat "$RESUME_DIR/.entropy" >/dev/urandom || fail "cannot reseed /dev/urandom"
    rm -f "$RESUME_DIR/.entropy"
    step reseeded
    step_clock "$epoch" || fail "cannot step clock"
    step clock
    printf '%s\n' "$room_id" >"$RESUME_DIR/identity"
    chmod 0644 "$RESUME_DIR/identity"
    stage_secrets "$RESUME_DIR/.secrets"
    step identity
    fresh_ssh_host_keys
    step hostkeys
    start_sshd || fail "cannot start sshd"
    step sshd

    printf 'ACK resume\n'
}

loop() {
    # A SINGLE long-lived socat that retries the connect internally
    # (retry/interval), rather than a shell loop respawning socat+sleep every
    # iteration. That matters for the base: the quiesce gate refuses any
    # non-baseline process to survive, and a respawning loop churns fresh
    # socat/sleep pids the gate would flag. This one process is captured in the
    # baseline and forks its EXEC child only on a successful connect — which
    # only happens post-restore, after quiesce. `-T 120` bounds a wedged
    # session; `forever` + `interval=2` polls until the host listener appears.
    exec socat -T 120 \
        VSOCK-CONNECT:2:5003,forever,interval=2,retry=1000000 \
        EXEC:'/sbin/rooms-resume-agent session' \
        >/dev/null 2>&1
}

case "${1:-}" in
    session) session ;;
    loop) loop ;;
    *) fail "usage: rooms-resume-agent {session|loop}" ;;
esac
