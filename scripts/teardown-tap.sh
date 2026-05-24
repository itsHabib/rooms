#!/usr/bin/env bash
# Remove the rooms TAP device + NAT/forward rules. Idempotent.

set -euo pipefail

TAP="${TAP:-tap-fc0}"

log() { printf '\033[1;34m[teardown-tap]\033[0m %s\n' "$*"; }

OUT_IFACE="$(ip route get 8.8.8.8 2>/dev/null | awk '/dev/ { for (i=1; i<NF; i++) if ($i == "dev") print $(i+1); exit }')"
OUT_IFACE="${OUT_IFACE:-eth0}"

if ip link show "$TAP" >/dev/null 2>&1; then
    log "removing $TAP"
    sudo ip link del "$TAP"
else
    log "$TAP not present; skipping"
fi

if sudo iptables -t nat -C POSTROUTING -o "$OUT_IFACE" -j MASQUERADE 2>/dev/null; then
    log "removing NAT rule"
    sudo iptables -t nat -D POSTROUTING -o "$OUT_IFACE" -j MASQUERADE
fi

if sudo iptables -C FORWARD -i "$TAP" -o "$OUT_IFACE" -j ACCEPT 2>/dev/null; then
    sudo iptables -D FORWARD -i "$TAP" -o "$OUT_IFACE" -j ACCEPT
fi
if sudo iptables -C FORWARD -i "$OUT_IFACE" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null; then
    sudo iptables -D FORWARD -i "$OUT_IFACE" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT
fi

log "done"
