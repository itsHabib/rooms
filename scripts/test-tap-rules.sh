#!/usr/bin/env bash
# Assert setup-tap.sh --host installs the ROOMS_FWD chain correctly, ordered,
# with the version/supernet marker, jumped from FORWARD position 1 — and that
# --host --teardown removes it cleanly and idempotently.
#
# Host-only: needs root/sudo, a routable outbound interface, and mutates live
# iptables. Run on rooms-host before merge — not in cloud CI.
#
# Usage:
#   sudo ./scripts/test-tap-rules.sh

set -euo pipefail

FWD_CHAIN="${ROOMS_FWD_CHAIN:-ROOMS_FWD}"
SUPERNET="${ROOMS_SUPERNET:-172.16.0.0/24}"
MARKER="${ROOMS_FWD_MARKER:-rooms:fwd:v1:172.16.0.0/24}"
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
    local haystack="$1" needle="$2" label="$3"
    if ! grep -Fq -- "$needle" <<<"$haystack"; then
        fatal "missing $label: expected '$needle'"
    fi
}

assert_not_grep() {
    local haystack="$1" needle="$2" label="$3"
    if grep -Fq -- "$needle" <<<"$haystack"; then
        fatal "unexpected $label: still present '$needle'"
    fi
}

chain_line() {
    local pattern="$1"
    iptables -S "$FWD_CHAIN" | grep -F -- "$pattern" | head -n1
}

assert_rule_before() {
    local earlier="$1" later="$2" label="$3"
    local dump earlier_line later_line
    dump="$(iptables -S "$FWD_CHAIN")"
    earlier_line="$(grep -Fn -- "$earlier" <<<"$dump" | head -n1 | cut -d: -f1)"
    later_line="$(grep -Fn -- "$later" <<<"$dump" | head -n1 | cut -d: -f1)"
    if [[ -z "$earlier_line" || -z "$later_line" ]]; then
        fatal "could not locate rules for ordering check: $label"
    fi
    if (( earlier_line >= later_line )); then
        fatal "$label: '$earlier' must appear before '$later' (lines $earlier_line vs $later_line)"
    fi
}

assert_rules_present() {
    local nat forward chain
    nat="$(iptables -t nat -S)"
    forward="$(iptables -S FORWARD)"
    chain="$(iptables -S "$FWD_CHAIN")"

    # FORWARD jumps into the chain at position 1 (ahead of any broad ACCEPT).
    local first_jump
    first_jump="$(iptables -S FORWARD | grep -F -- "-j $FWD_CHAIN" | head -n1)"
    if [[ "$first_jump" != "-A FORWARD -j $FWD_CHAIN" ]]; then
        fatal "expected '-A FORWARD -j $FWD_CHAIN' as the FORWARD jump, got '$first_jump'"
    fi
    local forward_first
    forward_first="$(grep -E '^-A FORWARD ' <<<"$forward" | head -n1)"
    if [[ "$forward_first" != "-A FORWARD -j $FWD_CHAIN" ]]; then
        fatal "$FWD_CHAIN jump must be the first FORWARD rule, got '$forward_first'"
    fi

    # Supernet-scoped NAT, no legacy unrestricted MASQUERADE.
    assert_grep "$nat" "-A POSTROUTING -s $SUPERNET -o $OUT_IFACE -j MASQUERADE" "source-restricted MASQUERADE"
    assert_not_grep "$nat" "-A POSTROUTING -o $OUT_IFACE -j MASQUERADE" "legacy unrestricted MASQUERADE"

    # Chain rules, all supernet-qualified.
    assert_grep "$chain" "-A $FWD_CHAIN -s $SUPERNET -d $SUPERNET -j DROP" "guest→guest isolation drop"
    assert_grep "$chain" "-A $FWD_CHAIN -s $SUPERNET -d 10.0.0.0/8 -j DROP" "10.0.0.0/8 drop"
    assert_grep "$chain" "-A $FWD_CHAIN -s $SUPERNET -d 192.168.0.0/16 -j DROP" "192.168.0.0/16 drop"
    assert_grep "$chain" "-A $FWD_CHAIN -s $SUPERNET -d 172.16.0.0/12 -j DROP" "172.16.0.0/12 drop"
    assert_grep "$chain" "-A $FWD_CHAIN -s $SUPERNET -o $OUT_IFACE -j ACCEPT" "egress accept"
    assert_grep "$chain" "--comment $MARKER" "version/supernet marker"

    # Isolation + LAN drops precede the egress accept; the marker tail is last.
    assert_rule_before "-s $SUPERNET -d $SUPERNET -j DROP" "-s $SUPERNET -o $OUT_IFACE -j ACCEPT" "isolation before egress"
    assert_rule_before "-d 10.0.0.0/8 -j DROP" "-s $SUPERNET -o $OUT_IFACE -j ACCEPT" "10/8 drop before egress"
    assert_rule_before "-s $SUPERNET -o $OUT_IFACE -j ACCEPT" "--comment $MARKER" "egress before marker tail"

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

    assert_not_grep "$forward" "-A FORWARD -j $FWD_CHAIN" "FORWARD jump"
    assert_not_grep "$nat" "-A POSTROUTING -s $SUPERNET -o $OUT_IFACE -j MASQUERADE" "source-restricted MASQUERADE"
    assert_not_grep "$nat" "-A POSTROUTING -o $OUT_IFACE -j MASQUERADE" "legacy unrestricted MASQUERADE"
    # The chain itself is gone (-S on a missing chain errors → empty capture).
    if iptables -S "$FWD_CHAIN" >/dev/null 2>&1; then
        fatal "$FWD_CHAIN chain still present after teardown"
    fi
}

log "running setup-tap.sh --host"
bash "$SCRIPT_DIR/setup-tap.sh" --host
assert_rules_present

log "re-running setup-tap.sh --host (idempotent)"
bash "$SCRIPT_DIR/setup-tap.sh" --host
assert_rules_present

log "running setup-tap.sh --host --teardown"
bash "$SCRIPT_DIR/setup-tap.sh" --host --teardown
assert_rules_absent

log "re-running teardown (idempotent no-op)"
bash "$SCRIPT_DIR/setup-tap.sh" --host --teardown
assert_rules_absent

log "all assertions passed"
