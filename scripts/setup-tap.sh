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
GUEST_NET="${GUEST_NET:-172.16.0.0/24}"
USER_NAME="${SUDO_USER:-$USER}"
STATE_DIR="${ROOMS_TAP_STATE_DIR:-/run/rooms}"
IP_FORWARD_STATE="$STATE_DIR/tap-ip-forward.prev"
OUT_FORWARD_STATE="$STATE_DIR/tap-out-forward.prev"
OUT_IFACE_STATE="$STATE_DIR/tap-out-iface"

log() { printf '\033[1;34m[setup-tap]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[setup-tap]\033[0m %s\n' "$*" >&2; exit 1; }

iptables_delete_while_present() {
    local table="$1"
    shift
    while sudo iptables -t "$table" -C "$@" 2>/dev/null; do
        sudo iptables -t "$table" -D "$@"
    done
}

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

# Record prior forwarding state once so teardown can restore it: the global
# ip_forward flag (which we deliberately do NOT flip) and the outbound
# interface's per-interface forwarding value.
sudo mkdir -p "$STATE_DIR"
if [[ ! -f "$IP_FORWARD_STATE" ]]; then
    log "recording prior net.ipv4.ip_forward"
    sysctl -n net.ipv4.ip_forward | sudo tee "$IP_FORWARD_STATE" >/dev/null
fi
if [[ ! -f "$OUT_FORWARD_STATE" ]]; then
    log "recording prior net.ipv4.conf.${OUT_IFACE}.forwarding"
    sysctl -n "net.ipv4.conf.${OUT_IFACE}.forwarding" | sudo tee "$OUT_FORWARD_STATE" >/dev/null
    printf '%s\n' "$OUT_IFACE" | sudo tee "$OUT_IFACE_STATE" >/dev/null
fi

# Scope forwarding to the TAP and outbound interface only — never flip the
# global ip_forward flag. Both directions need it: guest→internet is forwarded
# per the TAP's setting; the return path arrives on the outbound interface and
# is forwarded per its setting (without this the guest's egress replies are
# dropped even though the NAT and FORWARD rules look correct).
log "enabling IPv4 forwarding on $TAP and $OUT_IFACE"
sudo sysctl -w "net.ipv4.conf.${TAP}.forwarding=1" >/dev/null
sudo sysctl -w "net.ipv4.conf.${OUT_IFACE}.forwarding=1" >/dev/null

# Remove legacy unrestricted MASQUERADE and any prior rooms rule, then add
# the source-restricted NAT rule in a known-good state.
log "ensuring source-restricted NAT rule for $OUT_IFACE"
iptables_delete_while_present nat POSTROUTING -s "$GUEST_NET" -o "$OUT_IFACE" -j MASQUERADE
iptables_delete_while_present nat POSTROUTING -o "$OUT_IFACE" -j MASQUERADE
sudo iptables -t nat -A POSTROUTING -s "$GUEST_NET" -o "$OUT_IFACE" -j MASQUERADE

# Drop guest → RFC1918 before the egress accept. Re-add in order every run so
# upgrades from the permissive POC rules cannot leave DROP after ACCEPT.
log "ensuring guest→LAN blocks and egress forward rules"
iptables_delete_while_present filter FORWARD -i "$TAP" -d 192.168.0.0/16 -j DROP
iptables_delete_while_present filter FORWARD -i "$TAP" -d 10.0.0.0/8 -j DROP
iptables_delete_while_present filter FORWARD -i "$TAP" -d 172.16.0.0/12 -j DROP
iptables_delete_while_present filter FORWARD -i "$TAP" -o "$OUT_IFACE" -j ACCEPT
iptables_delete_while_present filter FORWARD -i "$OUT_IFACE" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT

sudo iptables -A FORWARD -i "$TAP" -d 192.168.0.0/16 -j DROP
sudo iptables -A FORWARD -i "$TAP" -d 10.0.0.0/8 -j DROP
sudo iptables -A FORWARD -i "$TAP" -d 172.16.0.0/12 -j DROP
sudo iptables -A FORWARD -i "$TAP" -o "$OUT_IFACE" -j ACCEPT
sudo iptables -A FORWARD -i "$OUT_IFACE" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT

log "done. $TAP is up at ${HOST_IP_CIDR%/*}; guest will reach internet via $OUT_IFACE."
