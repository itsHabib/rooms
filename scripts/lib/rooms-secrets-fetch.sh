#!/bin/sh
# /sbin/rooms-secrets-fetch — guest side of the vsock secrets hand-off
# (docs/features/vsock-secrets/spec.md). Runs once at boot, before sshd.
#
# Dumb by design: connect, stage, ack. The authoritative workload gate is
# host-side (the host proceeds only on the ack), so every local failure here
# just exits and lets the host fail the room closed.
#
# `$0 stage` is the socat EXEC child: the vsock stream is its stdin/stdout.
# The host frames the blob as `<decimal len>\n<blob>` and keeps the
# connection fully open — Firecracker's hybrid vsock does not deliver a
# host half-close as an EOF with a live reverse path, so EOF framing would
# eat the ack. The child reads the header, takes exactly len bytes, stages
# /run/rooms/secrets.env atomically (temp + rename) with its final
# mode/owner, and only then writes the OK ack — a staged file is the only
# thing acked.
set -e

RUN_DIR=/run/rooms
ENV_FILE="$RUN_DIR/secrets.env"
GUEST_USER=rooms
VSOCK_HOST_CID=2
VSOCK_PORT=5000

if [ "$1" = "stage" ]; then
    umask 077
    mkdir -p "$RUN_DIR"
    read -r blob_len
    head -c "$blob_len" > "$ENV_FILE.tmp"
    mv "$ENV_FILE.tmp" "$ENV_FILE"
    chown "$GUEST_USER:$GUEST_USER" "$RUN_DIR" "$ENV_FILE"
    printf 'OK\n'
    exit 0
fi

# Inert without a vsock device: a secretless run wires no device, and the
# image must keep booting outside rooms entirely.
[ -e /dev/vsock ] || exit 0

exec socat -T 10 "VSOCK-CONNECT:$VSOCK_HOST_CID:$VSOCK_PORT" EXEC:"$0 stage"
