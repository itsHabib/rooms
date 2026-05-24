#!/usr/bin/env bash
# Create the TAP device that Firecracker microVMs use as their NIC.
# Idempotent — safe to re-run. Requires sudo.
#
# Layout:
#   - tap-fc0   (host's end of the wire, 172.16.0.1)
#   - guest's eth0 will be 172.16.0.2 (configured via kernel cmdline)
#   - outbound packets get NAT'd through the host's real interface
#
# POC: one TAP, one room at a time. Per-room dynamic TAPs land in task #2.

set -euo pipefail

TAP="${TAP:-tap-fc0}"
HOST_IP_CIDR="${HOST_IP_CIDR:-172.16.0.1/24}"
USER_NAME="${SUDO_USER:-$USER}"

log() { printf '\033[1;34m[setup-tap]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[setup-tap]\033[0m %s\n' "$*" >&2; exit 1; }

if ! command -v iptables >/dev/null 2>&1; then
    fatal "iptables not found; install with: sudo apt install iptables"
fi

# Find the outbound interface dynamically (might be eth0, ens33, enp0s3, ...).
OUT_IFACE="$(ip route get 8.8.8.8 | awk '/dev/ { for (i=1; i<NF; i++) if ($i == "dev") print $(i+1); exit }')"
if [[ -z "${OUT_IFACE:-}" ]]; then
    fatal "could not detect outbound interface; check 'ip route get 8.8.8.8'"
fi
log "outbound interface: $OUT_IFACE"

# Tear down existing TAP if present (idempotent restart).
if ip link show "$TAP" >/dev/null 2>&1; then
    log "removing existing $TAP"
    sudo ip link del "$TAP"
fi

# Create TAP, owned by current user — so firecracker (running as $USER) can
# open it without needing CAP_NET_ADMIN or root.
log "creating $TAP owned by $USER_NAME"
sudo ip tuntap add "$TAP" mode tap user "$USER_NAME"
sudo ip addr add "$HOST_IP_CIDR" dev "$TAP"
sudo ip link set "$TAP" up

# Enable IPv4 forwarding (transient — resets on reboot, fine for POC).
log "enabling IPv4 forwarding"
sudo sysctl -w net.ipv4.ip_forward=1 >/dev/null

# NAT outbound traffic so the guest's 172.16.0.2 source IP gets rewritten
# to the host's outbound interface IP. Idempotent: -C checks existence;
# only -A if missing.
log "ensuring NAT rule for $OUT_IFACE"
if ! sudo iptables -t nat -C POSTROUTING -o "$OUT_IFACE" -j MASQUERADE 2>/dev/null; then
    sudo iptables -t nat -A POSTROUTING -o "$OUT_IFACE" -j MASQUERADE
fi

# Allow forwarding from TAP to outbound and return traffic back.
# Default Ubuntu iptables FORWARD policy is ACCEPT, so this is usually
# already permitted — but pin it explicitly.
if ! sudo iptables -C FORWARD -i "$TAP" -o "$OUT_IFACE" -j ACCEPT 2>/dev/null; then
    sudo iptables -A FORWARD -i "$TAP" -o "$OUT_IFACE" -j ACCEPT
fi
if ! sudo iptables -C FORWARD -i "$OUT_IFACE" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null; then
    sudo iptables -A FORWARD -i "$OUT_IFACE" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT
fi

log "done. $TAP is up at ${HOST_IP_CIDR%/*}; guest will reach internet via $OUT_IFACE."
