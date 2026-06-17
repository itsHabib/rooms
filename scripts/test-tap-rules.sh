#!/usr/bin/env bash
# Assert setup-tap.sh iptables rules are present, ordered, and removed by teardown.
#
# Host-only: needs root/sudo, a routable outbound interface, and mutates live
# iptables + tap-fc0. Run on rooms-host before merge — not in cloud CI.
#
# Usage:
#   sudo ./scripts/test-tap-rules.sh

set -euo pipefail

TAP="${TAP:-tap-fc0}"
GUEST_NET="${GUEST_NET:-172.16.0.0/24}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log()   { printf '\033[1;34m[test-tap-rules]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[test-tap-rules]\033[0m %s\n' "$*" >&2; exit 1; }

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    fatal "run as root: sudo $0"
fi

OUT_IFACE="$(ip route get 8.8.8.8 | awk '/dev/ { for (i=1; i<NF; i++) if ($i == "dev") print $(i+1); exit }')"
if [[ -z "${OUT_IFACE:-}" ]]; then
    fatal "could not detect outbound interface"
fi

assert_grep() {
    local haystack="$1"
    local needle="$2"
    local label="$3"
    if ! grep -Fq -- "$needle" <<<"$haystack"; then
        fatal "missing $label: expected '$needle'"
    fi
}

assert_not_grep() {
    local haystack="$1"
    local needle="$2"
    local label="$3"
    if grep -Fq -- "$needle" <<<"$haystack"; then
        fatal "unexpected $label: still present '$needle'"
    fi
}

forward_line() {
    local pattern="$1"
    iptables -S FORWARD | grep -F -- "$pattern" | head -n1
}

assert_rule_before() {
    local earlier="$1"
    local later="$2"
    local label="$3"
    local forward_dump
    forward_dump="$(iptables -S FORWARD)"
    local earlier_line later_line
    earlier_line="$(grep -Fn -- "$earlier" <<<"$forward_dump" | head -n1 | cut -d: -f1)"
    later_line="$(grep -Fn -- "$later" <<<"$forward_dump" | head -n1 | cut -d: -f1)"
    if [[ -z "$earlier_line" || -z "$later_line" ]]; then
        fatal "could not locate rules for ordering check: $label"
    fi
    if (( earlier_line >= later_line )); then
        fatal "$label: '$earlier' must appear before '$later' (lines $earlier_line vs $later_line)"
    fi
}

assert_rules_present() {
    local nat forward
    nat="$(iptables -t nat -S)"
    forward="$(iptables -S FORWARD)"

    assert_grep "$nat" "-A POSTROUTING -s $GUEST_NET -o $OUT_IFACE -j MASQUERADE" "source-restricted MASQUERADE"
    assert_not_grep "$nat" "-A POSTROUTING -o $OUT_IFACE -j MASQUERADE" "legacy unrestricted MASQUERADE"

    assert_grep "$forward" "-A FORWARD -d 192.168.0.0/16 -i $TAP -j DROP" "192.168.0.0/16 drop"
    assert_grep "$forward" "-A FORWARD -d 10.0.0.0/8 -i $TAP -j DROP" "10.0.0.0/8 drop"
    assert_grep "$forward" "-A FORWARD -d 172.16.0.0/12 -i $TAP -j DROP" "172.16.0.0/12 drop"
    assert_grep "$forward" "-A FORWARD -i $TAP -o $OUT_IFACE -j ACCEPT" "egress accept"

    local drop192 drop10 drop172 accept
    drop192="$(forward_line "-d 192.168.0.0/16 -i $TAP -j DROP")"
    drop10="$(forward_line "-d 10.0.0.0/8 -i $TAP -j DROP")"
    drop172="$(forward_line "-d 172.16.0.0/12 -i $TAP -j DROP")"
    accept="$(forward_line "-i $TAP -o $OUT_IFACE -j ACCEPT")"

    assert_rule_before "$drop192" "$accept" "192.168 drop before egress accept"
    assert_rule_before "$drop10" "$accept" "10/8 drop before egress accept"
    assert_rule_before "$drop172" "$accept" "172.16/12 drop before egress accept"

    local tap_forward
    tap_forward="$(sysctl -n "net.ipv4.conf.${TAP}.forwarding")"
    if [[ "$tap_forward" != "1" ]]; then
        fatal "expected net.ipv4.conf.${TAP}.forwarding=1, got $tap_forward"
    fi

    local out_forward
    out_forward="$(sysctl -n "net.ipv4.conf.${OUT_IFACE}.forwarding")"
    if [[ "$out_forward" != "1" ]]; then
        fatal "expected net.ipv4.conf.${OUT_IFACE}.forwarding=1, got $out_forward"
    fi
}

assert_rules_absent() {
    local nat forward
    nat="$(iptables -t nat -S)"
    forward="$(iptables -S FORWARD)"

    assert_not_grep "$nat" "-A POSTROUTING -s $GUEST_NET -o $OUT_IFACE -j MASQUERADE" "source-restricted MASQUERADE"
    assert_not_grep "$nat" "-A POSTROUTING -o $OUT_IFACE -j MASQUERADE" "legacy unrestricted MASQUERADE"
    assert_not_grep "$forward" "-A FORWARD -d 192.168.0.0/16 -i $TAP -j DROP" "192.168.0.0/16 drop"
    assert_not_grep "$forward" "-A FORWARD -d 10.0.0.0/8 -i $TAP -j DROP" "10.0.0.0/8 drop"
    assert_not_grep "$forward" "-A FORWARD -d 172.16.0.0/12 -i $TAP -j DROP" "172.16.0.0/12 drop"
    assert_not_grep "$forward" "-A FORWARD -i $TAP -o $OUT_IFACE -j ACCEPT" "egress accept"
    assert_not_grep "$forward" "-A FORWARD -i $OUT_IFACE -o $TAP -m state --state RELATED,ESTABLISHED -j ACCEPT" "return accept"
}

log "running setup-tap.sh"
bash "$SCRIPT_DIR/setup-tap.sh"
assert_rules_present

log "running teardown-tap.sh"
bash "$SCRIPT_DIR/teardown-tap.sh"
assert_rules_absent

log "re-running teardown-tap.sh (idempotent no-op)"
bash "$SCRIPT_DIR/teardown-tap.sh"
assert_rules_absent

log "all assertions passed"
