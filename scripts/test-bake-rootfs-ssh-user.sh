#!/usr/bin/env bash
# Fixture harness for bake-rootfs-ssh.sh's provision_rooms_user helper.
# Exercises flat-file user provisioning without a loop-mounted rootfs.
#
# CI-runnable: needs passwordless sudo (GitHub Actions ubuntu-latest).
#
# Usage:
#   bash scripts/test-bake-rootfs-ssh-user.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=scripts/bake-rootfs-ssh.sh
source "$SCRIPT_DIR/bake-rootfs-ssh.sh"

log()   { printf '\033[1;34m[test-bake-rootfs-ssh-user]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[test-bake-rootfs-ssh-user]\033[0m %s\n' "$*" >&2; exit 1; }

count_lines() {
    local file="$1" pattern="$2" n rc=0
    # `grep -c` prints the count but exits 1 on zero matches, which would abort
    # this script under `set -e` before assert_eq can report. Tolerate exit 1
    # (no match -> n holds the printed 0); let a real grep error (exit >=2) surface.
    n=$(grep -cE "$pattern" "$file") || rc=$?
    if (( rc >= 2 )); then
        return "$rc"
    fi
    printf '%s\n' "$n"
}

assert_eq() {
    local got="$1" want="$2" label="$3"
    if [[ "$got" != "$want" ]]; then
        fatal "$label: expected '$want', got '$got'"
    fi
}

assert_grep() {
    local file="$1" needle="$2" label="$3"
    if ! grep -qxF -- "$needle" "$file"; then
        fatal "missing $label in $file"
    fi
}

FIXTURE="$(mktemp -d -t rooms-bake-user-test.XXXXXX)"
trap 'rm -rf "$FIXTURE"' EXIT

PUBKEY="ssh-ed25519 AAAAfixture rooms-microvm-test"

log "creating stock quickstart-style fixture under $FIXTURE"
mkdir -p "$FIXTURE/etc" "$FIXTURE/home"
cat >"$FIXTURE/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
EOF
cat >"$FIXTURE/etc/shadow" <<'EOF'
root:*:19737:0:99999:7:::
daemon:*:19737:0:99999:7:::
EOF
cat >"$FIXTURE/etc/group" <<'EOF'
root:x:0:
daemon:x:1:
EOF

log "first provision run"
provision_rooms_user "$FIXTURE" "$PUBKEY"

assert_eq "$(count_lines "$FIXTURE/etc/passwd" '^rooms:')" "1" "passwd rooms lines"
assert_eq "$(count_lines "$FIXTURE/etc/shadow" '^rooms:')" "1" "shadow rooms lines"
assert_eq "$(count_lines "$FIXTURE/etc/group" '^rooms:')" "1" "group rooms lines"
assert_grep "$FIXTURE/home/rooms/.ssh/authorized_keys" "$PUBKEY" "pubkey"
assert_eq "$(stat -c '%a %u %g' "$FIXTURE/home/rooms")" "755 1000 1000" "home mode/owner"
assert_eq "$(stat -c '%a %u %g' "$FIXTURE/home/rooms/.ssh")" "700 1000 1000" ".ssh mode/owner"
assert_eq "$(stat -c '%a %u %g' "$FIXTURE/home/rooms/.ssh/authorized_keys")" "600 1000 1000" "authorized_keys mode/owner"

log "second provision run (idempotent)"
provision_rooms_user "$FIXTURE" "$PUBKEY"

assert_eq "$(count_lines "$FIXTURE/etc/passwd" '^rooms:')" "1" "passwd rooms lines after re-run"
assert_eq "$(count_lines "$FIXTURE/etc/shadow" '^rooms:')" "1" "shadow rooms lines after re-run"
assert_eq "$(count_lines "$FIXTURE/etc/group" '^rooms:')" "1" "group rooms lines after re-run"
assert_eq "$(grep -cF -- "$PUBKEY" "$FIXTURE/home/rooms/.ssh/authorized_keys")" "1" "authorized_keys pubkey count after re-run"
assert_eq "$(stat -c '%a %u %g' "$FIXTURE/home/rooms")" "755 1000 1000" "home mode/owner after re-run"

log "all assertions passed"
