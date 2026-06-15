#!/usr/bin/env bash
# Remove the rooms TAP device + NAT/forward rules. Idempotent.

set -euo pipefail

TAP="${TAP:-tap-fc0}"
GUEST_NET="${GUEST_NET:-172.16.0.0/24}"
STATE_DIR="${ROOMS_TAP_STATE_DIR:-/run/rooms}"
IP_FORWARD_STATE="$STATE_DIR/tap-ip-forward.prev"
OUT_FORWARD_STATE="$STATE_DIR/tap-out-forward.prev"
OUT_IFACE_STATE="$STATE_DIR/tap-out-iface"

log() { printf '\033[1;34m[teardown-tap]\033[0m %s\n' "$*"; }

iptables_delete_while_present() {
    local table="$1"
    shift
    while sudo iptables -t "$table" -C "$@" 2>/dev/null; do
        sudo iptables -t "$table" -D "$@"
    done
}

OUT_IFACE="$(ip route get 8.8.8.8 2>/dev/null | awk '/dev/ { for (i=1; i<NF; i++) if ($i == "dev") print $(i+1); exit }')"
OUT_IFACE="${OUT_IFACE:-eth0}"

if ip link show "$TAP" >/dev/null 2>&1; then
    log "disabling IPv4 forwarding on $TAP"
    sudo sysctl -w "net.ipv4.conf.${TAP}.forwarding=0" >/dev/null 2>&1 || true

    log "removing $TAP"
    sudo ip link del "$TAP"
else
    log "$TAP not present; skipping interface removal"
fi

log "removing NAT and forward rules"
iptables_delete_while_present nat POSTROUTING -s "$GUEST_NET" -o "$OUT_IFACE" -j MASQUERADE
iptables_delete_while_present nat POSTROUTING -o "$OUT_IFACE" -j MASQUERADE

iptables_delete_while_present filter FORWARD -i "$TAP" -d 192.168.0.0/16 -j DROP
iptables_delete_while_present filter FORWARD -i "$TAP" -d 10.0.0.0/8 -j DROP
iptables_delete_while_present filter FORWARD -i "$TAP" -d 172.16.0.0/12 -j DROP
iptables_delete_while_present filter FORWARD -i "$TAP" -o "$OUT_IFACE" -j ACCEPT
iptables_delete_while_present filter FORWARD -i "$OUT_IFACE" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT

if [[ -f "$IP_FORWARD_STATE" ]]; then
    prior="$(<"$IP_FORWARD_STATE")"
    log "restoring net.ipv4.ip_forward=$prior"
    sudo sysctl -w "net.ipv4.ip_forward=$prior" >/dev/null
    sudo rm -f "$IP_FORWARD_STATE"
fi

if [[ -f "$OUT_FORWARD_STATE" ]]; then
    out_iface_saved="$OUT_IFACE"
    [[ -f "$OUT_IFACE_STATE" ]] && out_iface_saved="$(<"$OUT_IFACE_STATE")"
    prior_out="$(<"$OUT_FORWARD_STATE")"
    log "restoring net.ipv4.conf.${out_iface_saved}.forwarding=$prior_out"
    sudo sysctl -w "net.ipv4.conf.${out_iface_saved}.forwarding=$prior_out" >/dev/null 2>&1 || true
    sudo rm -f "$OUT_FORWARD_STATE" "$OUT_IFACE_STATE"
fi

log "done"
