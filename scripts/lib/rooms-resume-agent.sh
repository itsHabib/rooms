#!/bin/sh
# Retained post-restore transport. This shell exists only long enough at boot
# to exec the static native receiver. The receiver itself is captured in the
# neutral snapshot, so resume performs no ELF exec (and therefore no cloned
# pre-reseed AT_RANDOM draw) before it mixes the host divergence nudge.
set -eu

fail() {
    printf 'ERR %s\n' "$*" >&2
    exit 1
}

loop() {
    exec /sbin/rooms-resume-apply --loop >/dev/null 2>&1
}

[ "${1:-}" = loop ] || fail "usage: rooms-resume-agent loop"
loop
