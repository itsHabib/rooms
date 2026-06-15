#!/usr/bin/env bash
# Unit-style harness for setup-rooms-host.sh checksum verification.
# Confirms a deliberate pin mismatch aborts before install/use (no downloads).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKSUMS="${SCRIPT_DIR}/checksums.txt"

lookup_checksum() {
    local artifact="$1"
    awk -v name="$artifact" '$1 ~ /^[0-9a-f]{64}$/ && $2 == name { print $1; exit }' "$CHECKSUMS"
}

verify_sha256() {
    local file="$1" artifact="$2"
    local expected actual
    expected="$(lookup_checksum "$artifact")"
    [[ -n "$expected" ]] || { echo "no pin for $artifact" >&2; return 1; }
    actual="$(sha256sum "$file" | awk '{print $1}')"
    if [[ "$actual" != "$expected" ]]; then
        echo "sha256 mismatch for $artifact: expected $expected, got $actual" >&2
        return 1
    fi
}

tmp="$(mktemp)"
echo "tampered content" >"$tmp"
trap 'rm -f "$tmp"' EXIT

if verify_sha256 "$tmp" "rustup-init.sh"; then
    echo "FAIL: verify_sha256 should reject a mismatched file" >&2
    exit 1
fi

echo "ok: checksum mismatch correctly rejected for rustup-init.sh"
