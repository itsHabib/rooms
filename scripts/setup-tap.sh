#!/usr/bin/env bash
# Install the once-per-host networking substrate for the rooms pool.
# Idempotent — safe to re-run. Requires sudo.
#
# The per-room taps (tap-fc<k>, one /30 per slot) are created in the boot path
# by the `rooms` binary, not here. This script installs only the host-once
# substrate every slot shares:
#
#   - the ROOMS_FWD filter chain: all rules source/dest-qualified by the
#     172.16.0.0/24 pool supernet, jumped from FORWARD position 1 so a
#     pre-existing broad ACCEPT can't preempt guest isolation.
#   - one supernet-scoped NAT MASQUERADE for egress.
#   - IPv4 forwarding on the outbound interface (the guest→internet return
#     path; per-tap forwarding is set per-slot by the binary).
#
# Usage:
#   sudo bash scripts/setup-tap.sh --host              install the substrate
#   sudo bash scripts/setup-tap.sh --host --teardown   remove it, restore sysctls

set -euo pipefail

FWD_CHAIN="${ROOMS_FWD_CHAIN:-ROOMS_FWD}"
SUPERNET="${ROOMS_SUPERNET:-172.16.0.0/24}"
# Marker doctor keys on: version + supernet. Bump the version when the chain's
# rule shape changes so `rooms doctor` flags hosts still on the old layout.
MARKER="${ROOMS_FWD_MARKER:-rooms:fwd:v1:172.16.0.0/24}"
STATE_DIR="${ROOMS_TAP_STATE_DIR:-/run/rooms}"
OUT_FORWARD_STATE="$STATE_DIR/host-out-forward.prev"
OUT_IFACE_STATE="$STATE_DIR/host-out-iface"

log()   { printf '\033[1;34m[setup-tap]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[setup-tap]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    cat >&2 <<'EOF'
usage: setup-tap.sh --host [--teardown]

  --host              install the once-per-host rooms networking substrate
  --host --teardown   remove the substrate and restore recorded sysctls

Per-room taps are created by the rooms binary on boot, not by this script.
EOF
    exit 2
}

iptables_delete_while_present() {
    local table="$1"
    shift
    while sudo iptables -t "$table" -C "$@" 2>/dev/null; do
        sudo iptables -t "$table" -D "$@"
    done
}

detect_out_iface() {
    ip route get 8.8.8.8 2>/dev/null \
        | awk '/dev/ { for (i=1; i<NF; i++) if ($i == "dev") print $(i+1); exit }'
}

install_host() {
    local out_iface="$1"

    # Fresh chain every run: create-if-missing, then flush so re-runs converge
    # on exactly the rule set below regardless of prior version.
    sudo iptables -N "$FWD_CHAIN" 2>/dev/null || true
    sudo iptables -F "$FWD_CHAIN"

    # All rules are qualified by the pool supernet, not an interface name — the
    # taps that carry this traffic don't exist yet at install time. A packet
    # that matches no rule falls through the chain back to FORWARD.
    #
    # guest → guest isolation: one slot can't reach another's /30.
    sudo iptables -A "$FWD_CHAIN" -s "$SUPERNET" -d "$SUPERNET" -j DROP
    # guest → host LAN (RFC1918): block before the egress accept.
    sudo iptables -A "$FWD_CHAIN" -s "$SUPERNET" -d 10.0.0.0/8 -j DROP
    sudo iptables -A "$FWD_CHAIN" -s "$SUPERNET" -d 192.168.0.0/16 -j DROP
    sudo iptables -A "$FWD_CHAIN" -s "$SUPERNET" -d 172.16.0.0/12 -j DROP
    # guest → internet: accept egress out the real interface.
    sudo iptables -A "$FWD_CHAIN" -s "$SUPERNET" -o "$out_iface" -j ACCEPT
    # internet → guest: return path for established flows, scoped to arrivals on
    # the outbound interface (keeps the ingress scope the per-tap rule had).
    sudo iptables -A "$FWD_CHAIN" -i "$out_iface" -d "$SUPERNET" \
        -m state --state RELATED,ESTABLISHED -j ACCEPT
    # Self-terminating default-deny tail. Doubles as the marker rule doctor
    # matches on: its comment carries the version + supernet.
    sudo iptables -A "$FWD_CHAIN" -s "$SUPERNET" \
        -m comment --comment "$MARKER" -j DROP

    # Jump into the chain from FORWARD position 1 — ahead of any pre-existing
    # broad ACCEPT that would otherwise let guest→guest slip past isolation.
    iptables_delete_while_present filter FORWARD -j "$FWD_CHAIN"
    sudo iptables -I FORWARD 1 -j "$FWD_CHAIN"

    # One supernet-scoped NAT rule for egress. Drop any legacy unrestricted or
    # prior rooms rule first so re-runs land in a known-good state.
    iptables_delete_while_present nat POSTROUTING -s "$SUPERNET" -o "$out_iface" -j MASQUERADE
    iptables_delete_while_present nat POSTROUTING -o "$out_iface" -j MASQUERADE
    sudo iptables -t nat -A POSTROUTING -s "$SUPERNET" -o "$out_iface" -j MASQUERADE

    # The guest→internet return path is forwarded per the outbound interface's
    # setting. Record the prior value (and the interface name) so teardown
    # restores exactly what we changed; never touch the global ip_forward flag.
    sudo mkdir -p "$STATE_DIR"
    if [[ ! -f "$OUT_FORWARD_STATE" ]]; then
        log "recording prior net.ipv4.conf.${out_iface}.forwarding"
        sysctl -n "net.ipv4.conf.${out_iface}.forwarding" | sudo tee "$OUT_FORWARD_STATE" >/dev/null
        printf '%s\n' "$out_iface" | sudo tee "$OUT_IFACE_STATE" >/dev/null
    fi
    log "enabling IPv4 forwarding on $out_iface"
    sudo sysctl -w "net.ipv4.conf.${out_iface}.forwarding=1" >/dev/null

    log "done. $FWD_CHAIN installed and scoped to $SUPERNET; egress via $out_iface."
}

teardown_host() {
    # Prefer the interface install actually used — the default route may have
    # moved since, and the sysctl was applied to the original interface.
    local out_iface
    out_iface="$(detect_out_iface)"
    out_iface="${out_iface:-eth0}"
    [[ -f "$OUT_IFACE_STATE" ]] && out_iface="$(<"$OUT_IFACE_STATE")"

    log "removing $FWD_CHAIN and NAT rule"
    iptables_delete_while_present filter FORWARD -j "$FWD_CHAIN"
    if sudo iptables -L "$FWD_CHAIN" >/dev/null 2>&1; then
        sudo iptables -F "$FWD_CHAIN"
        sudo iptables -X "$FWD_CHAIN"
    fi
    iptables_delete_while_present nat POSTROUTING -s "$SUPERNET" -o "$out_iface" -j MASQUERADE
    iptables_delete_while_present nat POSTROUTING -o "$out_iface" -j MASQUERADE

    if [[ -f "$OUT_FORWARD_STATE" ]]; then
        local prior_out
        prior_out="$(<"$OUT_FORWARD_STATE")"
        log "restoring net.ipv4.conf.${out_iface}.forwarding=$prior_out"
        sudo sysctl -w "net.ipv4.conf.${out_iface}.forwarding=$prior_out" >/dev/null 2>&1 || true
        sudo rm -f "$OUT_FORWARD_STATE" "$OUT_IFACE_STATE"
    fi

    log "done"
}

MODE=""
TEARDOWN=0
for arg in "$@"; do
    case "$arg" in
        --host) MODE="host" ;;
        --teardown) TEARDOWN=1 ;;
        -h|--help) usage ;;
        *) fatal "unknown argument: $arg (see --help)" ;;
    esac
done

[[ "$MODE" == "host" ]] || usage

if ! command -v iptables >/dev/null 2>&1; then
    fatal "iptables not found; install with: sudo apt install iptables"
fi

if [[ "$TEARDOWN" -eq 1 ]]; then
    teardown_host
    exit 0
fi

OUT_IFACE="$(detect_out_iface)"
[[ -n "${OUT_IFACE:-}" ]] || fatal "could not detect outbound interface; check 'ip route get 8.8.8.8'"
log "outbound interface: $OUT_IFACE"
install_host "$OUT_IFACE"
