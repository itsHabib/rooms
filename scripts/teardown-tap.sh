#!/usr/bin/env bash
# Remove the once-per-host rooms networking substrate. Idempotent.
#
# Thin wrapper kept for muscle memory: the substrate (ROOMS_FWD chain, NAT,
# recorded sysctls) is owned by setup-tap.sh, so teardown lives there too.
# Per-room taps are reaped by the rooms binary on room teardown/gc, not here.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "$SCRIPT_DIR/setup-tap.sh" --host --teardown "$@"
