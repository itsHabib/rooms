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
VETH_FWD_CHAIN="ROOMS_VETH_FWD"
VETH_SUPERNET="172.17.0.0/24"
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
# Start from a clean substrate so the forwarding value below is the value this
# test's first install must eventually restore.
bash "$SCRIPT_DIR/setup-tap.sh" --host --teardown
ORIGINAL_OUT_IFACE="$OUT_IFACE"
ORIGINAL_OUT_FORWARD="$(sysctl -n "net.ipv4.conf.${OUT_IFACE}.forwarding")"
TEST_OUT_IFACE="rooms-out-test"
OUT_IFACE_STATE="${ROOMS_TAP_STATE_DIR:-/run/rooms}/host-out-iface"
CLONENET_OWNER_DIR="/run/rooms/clonenet-owners"

cleanup_test_iface() {
    ROOMS_OUT_IFACE="$TEST_OUT_IFACE" bash "$SCRIPT_DIR/setup-tap.sh" --host --teardown >/dev/null 2>&1 || true
    iptables -t nat -D POSTROUTING -o "$ORIGINAL_OUT_IFACE" -j MASQUERADE >/dev/null 2>&1 || true
    ip netns del rooms-c63 >/dev/null 2>&1 || true
    ip link del veth-h62 >/dev/null 2>&1 || true
    ip link del veth-h63 >/dev/null 2>&1 || true
    rm -f "$CLONENET_OWNER_DIR/63"
    ip link del "$TEST_OUT_IFACE" >/dev/null 2>&1 || true
}
trap cleanup_test_iface EXIT

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
    # `iptables -S` prints the comment value quoted (`--comment "rooms:fwd:..."`),
    # so match the marker value itself — the same substring `doctor` keys on.
    assert_grep "$chain" "$MARKER" "version/supernet marker"

    # Isolation + LAN drops precede the egress accept; the marker tail is last.
    assert_rule_before "-s $SUPERNET -d $SUPERNET -j DROP" "-s $SUPERNET -o $OUT_IFACE -j ACCEPT" "isolation before egress"
    assert_rule_before "-d 10.0.0.0/8 -j DROP" "-s $SUPERNET -o $OUT_IFACE -j ACCEPT" "10/8 drop before egress"
    assert_rule_before "-s $SUPERNET -o $OUT_IFACE -j ACCEPT" "$MARKER" "egress before marker tail"

    local out_forward
    out_forward="$(sysctl -n "net.ipv4.conf.${OUT_IFACE}.forwarding")"
    if [[ "$out_forward" != "1" ]]; then
        fatal "expected net.ipv4.conf.${OUT_IFACE}.forwarding=1, got $out_forward"
    fi
}

assert_veth_rules_present() {
    local nat forward chain first second third antispoof_line drop_line accept_line
    nat="$(iptables -t nat -S)"
    forward="$(iptables -S FORWARD)"
    chain="$(iptables -S "$VETH_FWD_CHAIN")"

    first="$(grep -E '^-A FORWARD ' <<<"$forward" | sed -n '1p')"
    second="$(grep -E '^-A FORWARD ' <<<"$forward" | sed -n '2p')"
    third="$(grep -E '^-A FORWARD ' <<<"$forward" | sed -n '3p')"
    [[ "$first" == "-A FORWARD -j $FWD_CHAIN" ]] || fatal "$FWD_CHAIN must remain first"
    [[ "$second" == "-A FORWARD -i veth-h+ -j $VETH_FWD_CHAIN" ]] \
        || fatal "$VETH_FWD_CHAIN ingress jump must be second and interface-scoped"
    [[ "$third" == "-A FORWARD -o veth-h+ -j $VETH_FWD_CHAIN" ]] \
        || fatal "$VETH_FWD_CHAIN egress jump must be third and interface-scoped"
    assert_not_grep "$forward" "-A FORWARD -j $VETH_FWD_CHAIN" "broad veth FORWARD jump"

    assert_grep "$chain" "-A $VETH_FWD_CHAIN ! -s $VETH_SUPERNET -i veth-h+ -j DROP" "veth anti-spoof drop"
    assert_grep "$chain" "-A $VETH_FWD_CHAIN -s $VETH_SUPERNET -d $VETH_SUPERNET -j DROP" "veth cross-clone drop"
    assert_grep "$chain" "-A $VETH_FWD_CHAIN -s $VETH_SUPERNET -d 10.0.0.0/8 -j DROP" "veth 10/8 drop"
    assert_grep "$chain" "-A $VETH_FWD_CHAIN -s $VETH_SUPERNET -d 192.168.0.0/16 -j DROP" "veth 192.168/16 drop"
    assert_grep "$chain" "-A $VETH_FWD_CHAIN -s $VETH_SUPERNET -d 172.16.0.0/12 -j DROP" "veth 172.16/12 drop"
    assert_grep "$chain" "-A $VETH_FWD_CHAIN -s $VETH_SUPERNET -o $OUT_IFACE -j ACCEPT" "veth egress accept"
    assert_grep "$chain" "-A $VETH_FWD_CHAIN -d $VETH_SUPERNET -i $OUT_IFACE -m state --state RELATED,ESTABLISHED -j ACCEPT" "veth return accept"
    assert_grep "$nat" "-A POSTROUTING -s $VETH_SUPERNET -o $OUT_IFACE -j MASQUERADE" "veth host MASQUERADE"

    antispoof_line="$(grep -Fn -- "! -s $VETH_SUPERNET -i veth-h+ -j DROP" <<<"$chain" | head -n1 | cut -d: -f1)"
    drop_line="$(grep -Fn -- "-s $VETH_SUPERNET -d $VETH_SUPERNET -j DROP" <<<"$chain" | head -n1 | cut -d: -f1)"
    accept_line="$(grep -Fn -- "-s $VETH_SUPERNET -o $OUT_IFACE -j ACCEPT" <<<"$chain" | head -n1 | cut -d: -f1)"
    (( antispoof_line < drop_line )) || fatal "veth anti-spoof drop must precede cross-clone drop"
    (( drop_line < accept_line )) || fatal "veth cross-clone drop must precede egress accept"
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
    assert_not_grep "$forward" "-j $VETH_FWD_CHAIN" "veth FORWARD jumps"
    assert_not_grep "$nat" "-A POSTROUTING -s $VETH_SUPERNET -o $OUT_IFACE -j MASQUERADE" "veth MASQUERADE"
    if iptables -S "$VETH_FWD_CHAIN" >/dev/null 2>&1; then
        fatal "$VETH_FWD_CHAIN chain still present after teardown"
    fi
}

log "running setup-tap.sh --host"
bash "$SCRIPT_DIR/setup-tap.sh" --host
assert_rules_present
assert_veth_rules_present

log "re-running setup-tap.sh --host (idempotent)"
ip link del veth-h62 >/dev/null 2>&1 || true
ip link add veth-h62 type veth peer name veth-x62
before_foreign="$(iptables-save)"
if bash "$SCRIPT_DIR/setup-tap.sh" --host >/dev/null 2>&1; then
    fatal "setup accepted foreign veth-h62"
fi
[[ "$(iptables-save)" == "$before_foreign" ]] \
    || fatal "foreign veth preflight changed firewall state before refusing"
ip link del veth-h62

ip netns del rooms-c63 >/dev/null 2>&1 || true
ip link del veth-h63 >/dev/null 2>&1 || true
rm -f "$CLONENET_OWNER_DIR/63"
mkdir -p "$CLONENET_OWNER_DIR"
ln -s test-owner "$CLONENET_OWNER_DIR/63"
ip netns add rooms-c63
ip link add veth-h63 type veth peer name veth-g63
ip link set veth-g63 netns rooms-c63
ip addr add 172.17.0.253/30 dev veth-h63
ip link set veth-h63 up
bash "$SCRIPT_DIR/setup-tap.sh" --host
assert_rules_present
assert_veth_rules_present
assert_grep "$(iptables -S "$VETH_FWD_CHAIN")" \
    "-A $VETH_FWD_CHAIN ! -s 172.17.0.254/32 -i veth-h63 -j DROP" \
    "restored active-veth source binding"
ip netns del rooms-c63
ip link del veth-h63
rm -f "$CLONENET_OWNER_DIR/63"

log "re-running setup after an outbound-interface change"
iptables -t nat -A POSTROUTING -o "$ORIGINAL_OUT_IFACE" -j MASQUERADE
ip link del "$TEST_OUT_IFACE" >/dev/null 2>&1 || true
ip link add "$TEST_OUT_IFACE" type dummy
sysctl -w "net.ipv4.conf.${TEST_OUT_IFACE}.forwarding=0" >/dev/null
ROOMS_OUT_IFACE="$TEST_OUT_IFACE" bash "$SCRIPT_DIR/setup-tap.sh" --host

transition_nat="$(iptables -t nat -S)"
assert_not_grep "$transition_nat" "-s $SUPERNET -o $ORIGINAL_OUT_IFACE -j MASQUERADE" "old-interface flat MASQUERADE"
assert_not_grep "$transition_nat" "-s $VETH_SUPERNET -o $ORIGINAL_OUT_IFACE -j MASQUERADE" "old-interface veth MASQUERADE"
assert_grep "$transition_nat" "-A POSTROUTING -o $ORIGINAL_OUT_IFACE -j MASQUERADE" "unrelated old-interface MASQUERADE"
restored_forward="$(sysctl -n "net.ipv4.conf.${ORIGINAL_OUT_IFACE}.forwarding")"
[[ "$restored_forward" == "$ORIGINAL_OUT_FORWARD" ]] \
    || fatal "old-interface forwarding was not restored: got $restored_forward, want $ORIGINAL_OUT_FORWARD"
[[ "$(<"$OUT_IFACE_STATE")" == "$TEST_OUT_IFACE" ]] \
    || fatal "outbound-interface state did not move to $TEST_OUT_IFACE"

OUT_IFACE="$TEST_OUT_IFACE"
assert_rules_present
assert_veth_rules_present

log "running setup-tap.sh --host --teardown"
bash "$SCRIPT_DIR/setup-tap.sh" --host --teardown
assert_rules_absent
[[ "$(sysctl -n "net.ipv4.conf.${TEST_OUT_IFACE}.forwarding")" == "0" ]] \
    || fatal "test-interface forwarding was not restored"
ip link del "$TEST_OUT_IFACE"

log "re-running teardown (idempotent no-op)"
bash "$SCRIPT_DIR/setup-tap.sh" --host --teardown
assert_rules_absent

log "all assertions passed"
